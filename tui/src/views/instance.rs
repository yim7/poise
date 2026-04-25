use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::exposure_presentation::instance_exposure_annotation;
use crate::protocol::{
    ActivityLevelView, ExecutionBindingIntentView, ExecutionBindingStatusView, ExecutionStateView,
    ExecutionStatusView, TrackCommandType, TrackCommandView, TrackExecutionView,
};
use crate::signal::{exposure_signal, pnl_signal};
use crate::theme::Theme;
use crate::timestamp_display::format_local_timestamp_for_display;
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
    let execution_body_line_count = execution_body_line_count(&detail.execution);
    let sections = resolve_detail_layout(area, execution_body_line_count);

    let track = Paragraph::new(track_lines(detail, sections.mode))
        .block(Block::default().title("Track").borders(Borders::ALL));
    frame.render_widget(track, sections.track);

    if let Some(pnl_area) = sections.pnl {
        let pnl = Paragraph::new(pnl_lines(detail))
            .block(Block::default().title("PnL").borders(Borders::ALL));
        frame.render_widget(pnl, pnl_area);
    }

    let market = Paragraph::new(market_lines(detail, sections.mode))
        .block(Block::default().title("Market").borders(Borders::ALL));
    frame.render_widget(market, sections.market);

    if let Some(strategy_area) = sections.strategy {
        let strategy = Paragraph::new(strategy_lines(detail, sections.mode))
            .block(Block::default().title("Strategy").borders(Borders::ALL));
        frame.render_widget(strategy, strategy_area);
    }

    let execution_body_height = sections.execution.height.saturating_sub(2) as usize;
    let execution = Paragraph::new(execution_lines(
        &detail.execution,
        sections.mode,
        execution_body_height,
    ))
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
        let binding_count = format_binding_count(detail.execution.active_binding_count);
        lines.push(Line::from(Span::styled(
            format!(
                "{lifecycle} | {execution_state} | ATTENTION REQUIRED | {reason_summary} | gap {:.4} | {binding_count}",
                detail.execution.inventory_gap,
            ),
            Theme::execution_attention(),
        )));
    } else {
        lines.push(Line::from(format!(
            "{lifecycle} | {execution_state} | gap {:.4} | {}",
            detail.execution.inventory_gap,
            format_binding_count(detail.execution.active_binding_count)
        )));
    }

    lines.push(Line::from(format!(
        "{} | updated {}",
        detail.identity.instrument.venue,
        format_local_timestamp_for_display(&detail.status.lifecycle.updated_at)
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
                format_binding_count(detail.execution.active_binding_count)
            ),
            Theme::execution_attention(),
        ))
    } else {
        Line::from(format!(
            "{lifecycle} | {execution_state} | gap {:.4} | {}",
            detail.execution.inventory_gap,
            format_binding_count(detail.execution.active_binding_count)
        ))
    }
}

fn market_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Minimal) {
        return vec![format_exposure_line(
            detail.status.strategy_price,
            detail.position.current_exposure,
            detail.position.desired_exposure,
        )];
    }

    vec![
        Line::from(format!(
            "prices: strategy {} ({}) | mark {} | bid {} | ask {}",
            format_optional_price(detail.status.strategy_price),
            detail.status.strategy_price_status,
            format_optional_price(detail.market.mark_price),
            format_optional_price(detail.market.best_bid),
            format_optional_price(detail.market.best_ask)
        )),
        format_exposure_line(
            detail.status.strategy_price,
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
    vec![
        format_pnl_summary_line(
            detail.ledger.total_pnl,
            detail.ledger.unrealized_pnl,
            detail.ledger.gross_realized_pnl,
            detail.ledger.net_realized_pnl,
        ),
        format_pnl_cost_line(
            detail.ledger.trading_fee_cumulative,
            detail.ledger.funding_fee_cumulative,
        ),
        format_ledger_gap_line(&detail.ledger.unresolved_gaps),
    ]
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
        format_local_timestamp_for_display(ts),
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

fn execution_body_line_count(execution: &TrackExecutionView) -> usize {
    let alert_lines = usize::from(matches!(
        execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ));
    let binding_lines = execution.bindings.len();

    match (alert_lines, binding_lines) {
        (0, 0) => 1,
        (alerts, 0) => alerts + 1,
        (alerts, bindings) => alerts + bindings,
    }
}

fn execution_lines(
    execution: &TrackExecutionView,
    mode: DetailLayoutMode,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let binding_details = execution
        .bindings
        .iter()
        .map(format_binding_detail)
        .collect::<Vec<_>>();

    if matches!(mode, DetailLayoutMode::Minimal) {
        return limit_execution_lines(
            minimal_execution_lines(execution, &binding_details),
            max_lines,
        );
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

    if lines.is_empty() && binding_details.is_empty() {
        return limit_execution_lines(vec![Line::from("no active bindings")], max_lines);
    }

    if matches!(mode, DetailLayoutMode::Compact) {
        let execution_summary = format_compact_execution_summary(execution);
        if let Some(summary) = execution_summary {
            lines.push(Line::from(summary));
        }
    } else {
        if !binding_details.is_empty() {
            lines.extend(binding_details.into_iter().map(Line::from));
        } else if !lines.is_empty() {
            lines.push(Line::from("bindings: none"));
        }
    }

    limit_execution_lines(lines, max_lines)
}

fn limit_execution_lines(mut lines: Vec<Line<'static>>, max_lines: usize) -> Vec<Line<'static>> {
    if max_lines == 0 || lines.len() <= max_lines {
        return lines;
    }
    if max_lines == 1 {
        return vec![Line::from(format!("+{} more execution lines", lines.len()))];
    }

    let hidden = lines.len() - max_lines + 1;
    lines.truncate(max_lines - 1);
    lines.push(Line::from(format!("+{hidden} more execution lines")));
    lines
}

fn minimal_execution_lines(
    execution: &TrackExecutionView,
    binding_details: &[String],
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

    if let Some(summary) = format_compact_execution_summary(execution) {
        return vec![Line::from(summary)];
    }

    if binding_details.is_empty() {
        return vec![Line::from("no active bindings")];
    }

    vec![Line::from("execution detail unavailable")]
}

fn format_compact_execution_summary(execution: &TrackExecutionView) -> Option<String> {
    let bindings = compact_binding_summary(execution);

    bindings
}

fn compact_binding_summary(execution: &TrackExecutionView) -> Option<String> {
    match execution.bindings.as_slice() {
        [] => None,
        [binding] => Some(format!("binding: {}", compact_binding_label(binding))),
        bindings => Some(format!(
            "bindings {} | {}",
            bindings.len(),
            compact_binding_label(&bindings[0])
        )),
    }
}

fn format_binding_count(count: u32) -> String {
    format!("bindings {count}")
}

fn format_binding_detail(binding: &poise_protocol::ExecutionBindingView) -> String {
    let order = binding
        .order
        .as_ref()
        .map(|order| format!("{} {:.4} @ {:.4}", order.side, order.quantity, order.price))
        .unwrap_or_else(|| "no order".to_string());
    format!(
        "{} {} {} {}",
        binding.label,
        format_binding_status(binding.status),
        format_binding_intent(binding.intent),
        order
    )
}

fn compact_binding_label(binding: &poise_protocol::ExecutionBindingView) -> String {
    let order = binding
        .order
        .as_ref()
        .map(|order| format!("{} {:.4} @ {:.4}", order.side, order.quantity, order.price))
        .unwrap_or_else(|| "no order".to_string());
    format!("{} {order}", binding.label)
}

fn format_binding_status(value: ExecutionBindingStatusView) -> &'static str {
    match value {
        ExecutionBindingStatusView::SubmitPending => "submit_pending",
        ExecutionBindingStatusView::Working => "working",
        ExecutionBindingStatusView::CancelPending => "cancel_pending",
    }
}

fn format_binding_intent(value: ExecutionBindingIntentView) -> &'static str {
    match value {
        ExecutionBindingIntentView::IncreaseInventory => "increase_inventory",
        ExecutionBindingIntentView::DecreaseInventory => "decrease_inventory",
    }
}

fn format_optional_price(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_exposure_line(
    _strategy_price: Option<f64>,
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
    unrealized_pnl: f64,
    gross_realized_pnl: f64,
    net_realized_pnl: f64,
) -> Line<'static> {
    let total = pnl_signal(total_pnl);
    let unrealized = pnl_signal(unrealized_pnl);
    let gross_realized = pnl_signal(gross_realized_pnl);
    let net_realized = pnl_signal(net_realized_pnl);

    Line::from(vec![
        Span::raw("total "),
        Span::styled(total.text, total.style),
        Span::raw(" | unrealized "),
        Span::styled(unrealized.text, unrealized.style),
        Span::raw(" | gross realized "),
        Span::styled(gross_realized.text, gross_realized.style),
        Span::raw(" | net realized "),
        Span::styled(net_realized.text, net_realized.style),
    ])
}

fn format_pnl_cost_line(trading_fee_cumulative: f64, funding_fee_cumulative: f64) -> Line<'static> {
    let trading_fee = pnl_signal(-trading_fee_cumulative);
    let funding_fee = pnl_signal(funding_fee_cumulative);

    Line::from(vec![
        Span::raw("fee cumulative "),
        Span::styled(trading_fee.text, trading_fee.style),
        Span::raw(" | funding cumulative "),
        Span::styled(funding_fee.text, funding_fee.style),
    ])
}

fn format_ledger_gap_line(gaps: &[crate::protocol::TrackLedgerGapView]) -> Line<'static> {
    if gaps.is_empty() {
        return Line::from("ledger gaps: none");
    }

    let first = &gaps[0];
    let mut summary = format!(
        "ledger gaps: {} ({})",
        format_ledger_gap_reason(first.reason),
        first.observed_at
    );
    if gaps.len() > 1 {
        summary.push_str(&format!(" | +{} more", gaps.len() - 1));
    }

    Line::from(summary)
}

fn format_ledger_gap_reason(reason: crate::protocol::TrackLedgerGapReasonView) -> &'static str {
    match reason {
        crate::protocol::TrackLedgerGapReasonView::UnsupportedCommissionAsset => {
            "unsupported commission asset"
        }
        crate::protocol::TrackLedgerGapReasonView::MissingCommissionAsset => {
            "missing commission asset"
        }
        crate::protocol::TrackLedgerGapReasonView::MissingSymbol => "missing symbol",
        crate::protocol::TrackLedgerGapReasonView::UnsupportedFundingAsset => {
            "unsupported funding asset"
        }
        crate::protocol::TrackLedgerGapReasonView::Unknown => "unknown ledger gap",
    }
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

#[cfg(test)]
mod tests {
    use crate::app::{App, View};
    use crate::protocol::{
        ExecutionStatusView, TrackCommandType, TrackCommandView, TrackDetailView,
        TrackDiagnosticsView,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{execution_lines, pnl_lines, render};
    use crate::timestamp_display::format_local_timestamp_for_display;
    use crate::views::instance_layout::DetailLayoutMode;

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

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
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

        let text = render_text_with_size(detail, 160, 36);

        assert!(text.contains("Track"));
        assert!(text.contains("PnL"));
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
        assert!(text.contains("Trace"));
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
        let pnl_text = pnl_lines(&detail).iter().map(line_text).collect::<Vec<_>>();
        let text = render_text_with_size(detail, 180, 36);

        assert!(text.contains("Market"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("PnL"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Trace"));
        assert!(text.contains("Activity"));
        assert!(!text.contains("Commands"));
        assert!(text.contains("commands: p pause"));
        assert!(pnl_text[0].contains("total ↑ +1229.00"));
        assert!(pnl_text[0].contains("unrealized ↑ +265.20"));
        assert!(pnl_text[0].contains("gross realized ↑ +980.10"));
        assert!(pnl_text[0].contains("net realized ↑ +963.80"));
        assert!(pnl_text[1].contains("fee cumulative ↓ -12.30"));
        assert!(pnl_text[1].contains("funding cumulative ↓ -4.00"));
        assert_eq!(pnl_text[2], "ledger gaps: none");
        assert!(text.contains(
            "prices: strategy 101.2500 (live) | mark 101.3000 | bid 101.2000 | ask 101.4000"
        ));
        assert!(text.contains("exposure: 3.5000 → 4.0000 [↑ +0.5000]"));
        assert!(text.contains("lower/upper: 90.0000 / 110.0000"));
        assert!(text.contains("units 8.0000/8.0000 | notional 375.0000"));
        assert!(text.contains("maker 1 submit_pending increase_inventory buy 0.0100 @ 100.5000"));
        assert!(!text.contains("replacement gate"));
        assert!(!text.contains("Diagnostics"));
        assert!(!text.contains("client-1"));
    }

    #[test]
    fn renders_each_execution_binding_on_its_own_detail_line() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        let template = detail.execution.bindings[0].clone();
        detail.execution.bindings = (1..=6)
            .map(|index| {
                let mut binding = template.clone();
                binding.label = format!("maker {index}");
                binding
            })
            .collect();

        let lines = execution_lines(&detail.execution, DetailLayoutMode::Standard, 6)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("maker "))
                .count(),
            6
        );
        assert!(
            !lines.iter().any(|line| line.contains(" | maker ")),
            "bindings should not be packed into one horizontally truncated line"
        );

        let text = render_text_with_size(detail, 160, 36);
        assert!(text.contains("maker 6 submit_pending increase_inventory buy 0.0100 @ 100.5000"));
    }

    #[test]
    fn execution_lines_reports_hidden_bindings_when_panel_is_bounded() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        let template = detail.execution.bindings[0].clone();
        detail.execution.bindings = (1..=8)
            .map(|index| {
                let mut binding = template.clone();
                binding.label = format!("maker {index}");
                binding
            })
            .collect();

        let lines = execution_lines(&detail.execution, DetailLayoutMode::Standard, 5)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 5);
        assert!(lines[4].contains("+4 more execution lines"));
    }

    #[test]
    fn renders_market_block_with_strategy_mark_and_top_of_book_prices() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text_with_size(detail, 180, 36);

        assert!(text.contains(
            "prices: strategy 101.2500 (live) | mark 101.3000 | bid 101.2000 | ask 101.4000"
        ));
    }

    #[test]
    fn renders_flatten_trigger_policy_name() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.strategy.out_of_band_policy = serde_json::from_value(serde_json::json!({
            "flatten": {
                "trigger": { "flatten_confirm": { "bps": 500 } },
                "recover": { "reentry_confirm": { "bps": 500 } }
            }
        }))
        .unwrap();
        detail.status.lifecycle.status = serde_json::from_str("\"manual_flattening\"").unwrap();

        let text = render_text_with_size(detail, 180, 36);

        assert!(text.contains("manual_flattening"));
        assert!(text.contains("out of band flatten"));
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
        detail.ledger.total_pnl = -123456789.12;
        detail.ledger.gross_realized_pnl = 987654321.99;
        detail.ledger.net_realized_pnl = 876543210.88;
        detail.ledger.unrealized_pnl = -111111111.11;
        detail.ledger.trading_fee_cumulative = 12.34;
        detail.ledger.funding_fee_cumulative = -5.67;

        let pnl_text = pnl_lines(&detail).iter().map(line_text).collect::<Vec<_>>();

        assert!(pnl_text[0].contains("total ↓ -123456789.12"));
        assert!(pnl_text[0].contains("unrealized ↓ -111111111.11"));
        assert!(pnl_text[0].contains("gross realized ↑ +987654321.99"));
        assert!(pnl_text[0].contains("net realized ↑ +876543210.88"));
        assert!(pnl_text[1].contains("fee cumulative ↓ -12.34"));
        assert!(pnl_text[1].contains("funding cumulative ↓ -5.67"));
    }

    #[test]
    fn renders_ledger_gap_summary_when_unresolved_gap_exists() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.ledger.unresolved_gaps = vec![crate::protocol::TrackLedgerGapView {
            gap_key: "gap-1".to_string(),
            reason: crate::protocol::TrackLedgerGapReasonView::UnsupportedCommissionAsset,
            observed_at: "2026-04-06T10:00:00Z".to_string(),
        }];

        let pnl_text = pnl_lines(&detail).iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(
            pnl_text[2],
            "ledger gaps: unsupported commission asset (2026-04-06T10:00:00Z)"
        );
    }

    #[test]
    fn renders_activity_timestamp_in_local_time() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text(detail.clone());
        let original_ts = detail.activity[0].ts.clone();
        let expected_local = format_local_timestamp_for_display(&original_ts);

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
    fn renders_track_timestamps_in_local_time() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text(detail.clone());
        let expected_updated_at =
            format_local_timestamp_for_display(&detail.status.lifecycle.updated_at);

        assert!(text.contains(&format!("updated {expected_updated_at}")));
        assert!(!text.contains(&detail.status.lifecycle.updated_at));
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
    fn renders_attention_summary_without_hiding_gap() {
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
        assert!(text.contains("gap 0.5000 | bindings 1"));
        assert!(text.contains("alerts: recovery anomaly: duplicate_live_orders | market data stale | insufficient account margin"));
    }
}
