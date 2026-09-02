use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::exposure_presentation::instance_exposure_annotation;
use crate::protocol::{
    AcquisitionResetReasonView, AcquisitionSessionView, ActivityLevelView, DecisionReasonView,
    ExecutionBindingIntentView, ExecutionBindingStatusView, ExecutionStateView,
    ExecutionStatusView, InventoryDecisionView, RiskAcquisitionDirectionView, RiskAcquisitionView,
    TrackCommandType, TrackCommandView,
};
use crate::signal::{exposure_signal, pnl_signal};
use crate::theme::Theme;
use crate::timestamp_display::format_local_timestamp_for_display;
use crate::views::instance_layout::{
    DetailLayoutMode, resolve_detail_layout, resolve_trace_layout,
};

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(detail) = app.current_track_detail() else {
        let empty = Paragraph::new("No track detail loaded")
            .block(Block::default().title("Instance").borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };
    let execution_body_line_count = execution_body_line_count(detail);
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
        detail,
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
    let inventory_gap = format_optional_exposure(inventory_gap(detail));

    if matches!(
        detail.execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        let reason_summary = attention_summary(&detail.execution.attention_reasons);
        let binding_count = format_binding_count(detail.execution.active_binding_count);
        lines.push(Line::from(Span::styled(
            format!(
                "{lifecycle} | {execution_state} | ATTENTION REQUIRED | {reason_summary} | gap {inventory_gap} | {binding_count}",
            ),
            Theme::execution_attention(),
        )));
    } else {
        lines.push(Line::from(format!(
            "{lifecycle} | {execution_state} | gap {inventory_gap} | {}",
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
    let inventory_gap = format_optional_exposure(inventory_gap(detail));

    if matches!(
        detail.execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        Line::from(Span::styled(
            format!(
                "{lifecycle} | {execution_state} | ATTENTION | gap {inventory_gap} | {}",
                format_binding_count(detail.execution.active_binding_count)
            ),
            Theme::execution_attention(),
        ))
    } else {
        Line::from(format!(
            "{lifecycle} | {execution_state} | gap {inventory_gap} | {}",
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
            detail.decision.execution_target,
            detail.execution.risk_acquisition.as_ref(),
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
        Line::from(format!(
            "position: qty {:.6} | notional {} {}",
            detail.position.quantity,
            format_optional_amount(detail.position.notional),
            detail.position.notional_asset
        )),
        format_exposure_line(
            detail.status.strategy_price,
            detail.position.current_exposure,
            detail.decision.execution_target,
            detail.execution.risk_acquisition.as_ref(),
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
            &detail.pnl.pnl_asset,
            detail.pnl.total_pnl,
            detail.pnl.unrealized_pnl,
            detail.pnl.gross_realized_pnl,
            detail.pnl.net_realized_pnl,
        ),
        format_pnl_cost_line(
            &detail.pnl.pnl_asset,
            detail.pnl.trading_fee_cumulative,
            detail.pnl.funding_fee_cumulative,
        ),
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

fn execution_body_line_count(detail: &crate::protocol::TrackDetailView) -> usize {
    let execution = &detail.execution;
    let alert_lines = usize::from(matches!(
        execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ));
    let target_lines = 2;
    let acquisition_session_lines = usize::from(execution.acquisition_session.is_some());
    let risk_acquisition_lines = execution.risk_acquisition.as_ref().map_or(0, |_| 2);
    let binding_lines = if execution.bindings.is_empty() && execution.risk_acquisition.is_none() {
        1
    } else {
        execution.bindings.len()
    };

    target_lines + alert_lines + acquisition_session_lines + risk_acquisition_lines + binding_lines
}

fn execution_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let execution = &detail.execution;
    let binding_details = execution
        .bindings
        .iter()
        .map(format_binding_detail)
        .collect::<Vec<_>>();

    if matches!(mode, DetailLayoutMode::Compact | DetailLayoutMode::Minimal) {
        return vec![format_compact_decision_line(&detail.decision)];
    }

    let mut lines = format_decision_lines(&detail.decision);

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

    if let Some(acquisition_session) = &execution.acquisition_session {
        lines.push(format_acquisition_session_line(acquisition_session));
    }

    if let Some(risk_acquisition) = &execution.risk_acquisition {
        lines.extend(format_risk_acquisition_lines(risk_acquisition));
    }

    if !binding_details.is_empty() {
        lines.extend(binding_details.into_iter().map(Line::from));
    } else if !lines.is_empty() && execution.risk_acquisition.is_none() {
        lines.push(Line::from("bindings: none"));
    }

    limit_execution_lines(lines, max_lines)
}

fn limit_execution_lines(mut lines: Vec<Line<'static>>, max_lines: usize) -> Vec<Line<'static>> {
    if max_lines == 0 || lines.len() <= max_lines {
        return lines;
    }
    if max_lines == 1 {
        return lines.into_iter().take(1).collect();
    }

    let hidden = lines.len() - max_lines + 1;
    lines.truncate(max_lines - 1);
    lines.push(Line::from(format!("+{hidden} more execution lines")));
    lines
}

fn format_decision_lines(decision: &InventoryDecisionView) -> Vec<Line<'static>> {
    let revision = format_decision_revision(decision.revision);
    let reason = format_decision_reason(decision.reason);
    let mut lines = vec![Line::from(format!("decision {revision} | reason {reason}"))];
    if decision_targets_are_unavailable(decision) {
        lines.push(Line::from("targets unavailable"));
    } else {
        lines.push(Line::from(format!(
            "targets: policy {} | approved {} | frontier {} | execution {}",
            format_optional_signed_exposure(decision.policy_target),
            format_optional_signed_exposure(decision.risk_approved_target),
            format_optional_signed_exposure(decision.acquisition_frontier),
            format_optional_signed_exposure(decision.execution_target),
        )));
    }
    lines
}

fn format_compact_decision_line(decision: &InventoryDecisionView) -> Line<'static> {
    let revision = format_decision_revision(decision.revision);
    let reason = format_decision_reason(decision.reason);
    if decision_targets_are_unavailable(decision) {
        Line::from(format!("{revision} {reason} | targets unavailable"))
    } else {
        Line::from(format!(
            "{revision} {reason} | policy {} approved {} frontier {} exec {}",
            format_optional_compact_target(decision.policy_target),
            format_optional_compact_target(decision.risk_approved_target),
            format_optional_compact_target(decision.acquisition_frontier),
            format_optional_compact_target(decision.execution_target),
        ))
    }
}

fn decision_targets_are_unavailable(decision: &InventoryDecisionView) -> bool {
    decision.policy_target.is_none()
        && decision.risk_approved_target.is_none()
        && decision.acquisition_frontier.is_none()
        && decision.execution_target.is_none()
}

fn format_optional_compact_target(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:+.2}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_decision_revision(revision: Option<u64>) -> String {
    revision
        .map(|revision| format!("r{revision}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_decision_reason(reason: DecisionReasonView) -> &'static str {
    match reason {
        DecisionReasonView::WaitingMarketData => "waiting_market_data",
        DecisionReasonView::Paused => "paused",
        DecisionReasonView::RiskInputInvalid => "risk_input_invalid",
        DecisionReasonView::BandPolicyBlocked => "band_policy_blocked",
        DecisionReasonView::AtExecutionTarget => "at_execution_target",
        DecisionReasonView::WaitingRiskRelease => "waiting_risk_release",
        DecisionReasonView::AccountCapacityBlocked => "account_capacity_blocked",
        DecisionReasonView::PriceGateBlocked => "price_gate_blocked",
        DecisionReasonView::BelowRebalance => "below_rebalance",
        DecisionReasonView::BelowVenueMinimum => "below_venue_minimum",
        DecisionReasonView::CoveredByWorkingOrder => "covered_by_working_order",
        DecisionReasonView::RecoveryRequired => "recovery_required",
        DecisionReasonView::WaitingCancel => "waiting_cancel",
        DecisionReasonView::SubmitPlanned => "submit_planned",
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

fn format_risk_acquisition_lines(risk_acquisition: &RiskAcquisitionView) -> Vec<Line<'static>> {
    vec![
        Line::from(format!(
            "acq: {} | backlog {} | release {} -> {}",
            format_risk_direction(risk_acquisition.direction),
            format_signed_exposure(risk_acquisition.backlog_units),
            format_signed_exposure(risk_acquisition.next_release_units),
            format_signed_exposure(risk_acquisition.next_release_target),
        )),
        Line::from(format!(
            "unlock: {} | anchor {:.4}/{}",
            format_release_trigger_conditions(risk_acquisition),
            risk_acquisition.release_anchor_price,
            format_signed_exposure(risk_acquisition.release_anchor_target),
        )),
    ]
}

fn format_acquisition_session_line(session: &AcquisitionSessionView) -> Line<'static> {
    let reset_reason = match session.acquisition_reset_reason {
        AcquisitionResetReasonView::FreshSessionStart => "fresh_session_start",
    };
    Line::from(format!(
        "acq session: {reset_reason} @ {} | initial frontier {} | initial release {}",
        format_local_timestamp_for_display(&session.acquisition_session_started_at),
        format_optional_signed_exposure(session.initial_frontier),
        format_optional_signed_exposure(session.initial_release_units),
    ))
}

fn format_release_trigger_conditions(risk_acquisition: &RiskAcquisitionView) -> String {
    let mut conditions = Vec::new();
    if let Some(condition) = format_price_release_condition(risk_acquisition) {
        conditions.push(condition);
    }
    if let Some(condition) = format_stale_release_condition(risk_acquisition) {
        conditions.push(condition);
    }
    if conditions.is_empty() {
        return "none".to_string();
    }
    conditions.join(" or ")
}

fn format_price_release_condition(risk_acquisition: &RiskAcquisitionView) -> Option<String> {
    let price = risk_acquisition.next_advantage_price?;
    let comparison = match risk_acquisition.direction {
        RiskAcquisitionDirectionView::Long => "<=",
        RiskAcquisitionDirectionView::Short => ">=",
    };
    Some(format!("price {comparison} {price:.4}"))
}

fn format_stale_release_condition(risk_acquisition: &RiskAcquisitionView) -> Option<String> {
    if risk_acquisition.stale_release_minutes <= f64::EPSILON {
        return None;
    }
    let elapsed = risk_acquisition
        .stale_release_elapsed_minutes
        .max(0.0)
        .min(risk_acquisition.stale_release_minutes);
    let remaining = (risk_acquisition.stale_release_minutes - elapsed).max(0.0);
    if remaining <= f64::EPSILON {
        return Some("stale due".to_string());
    }
    Some(format!("stale in {}m", format_minutes(remaining)))
}

fn format_minutes(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}", value)
    } else {
        format!("{:.1}", value)
    }
}

fn format_risk_direction(direction: RiskAcquisitionDirectionView) -> &'static str {
    match direction {
        RiskAcquisitionDirectionView::Long => "long",
        RiskAcquisitionDirectionView::Short => "short",
    }
}

fn format_signed_exposure(value: f64) -> String {
    format!("{value:+.4}")
}

fn format_optional_signed_exposure(value: Option<f64>) -> String {
    value
        .map(format_signed_exposure)
        .unwrap_or_else(|| "-".to_string())
}

fn format_optional_exposure(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
}

fn inventory_gap(detail: &crate::protocol::TrackDetailView) -> Option<f64> {
    detail
        .decision
        .execution_target
        .map(|target| target - detail.position.current_exposure)
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

fn format_amount(value: f64) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let absolute = format!("{:.2}", value.abs());
    let (integer, fraction) = absolute
        .split_once('.')
        .expect("fixed precision amount should contain decimal point");

    format!("{sign}{}.{fraction}", group_digits(integer))
}

fn format_optional_amount(value: Option<f64>) -> String {
    value.map(format_amount).unwrap_or_else(|| "-".to_string())
}

fn group_digits(value: &str) -> String {
    let mut grouped_reversed = String::with_capacity(value.len() + value.len() / 3);
    for (index, ch) in value.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped_reversed.push(',');
        }
        grouped_reversed.push(ch);
    }
    grouped_reversed.chars().rev().collect()
}

fn format_exposure_line(
    _strategy_price: Option<f64>,
    current: f64,
    target: Option<f64>,
    risk_acquisition: Option<&RiskAcquisitionView>,
) -> Line<'static> {
    let signal = exposure_signal(current, target);
    let annotation = instance_exposure_annotation(signal);
    let target_text = target
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string());

    let mut spans = vec![
        Span::raw(format!("exposure: {current:.4} → {target_text} ")),
        Span::styled(annotation.text, annotation.style),
    ];
    if let Some(risk_acquisition) = risk_acquisition {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!(
                "[acq backlog {}]",
                format_signed_exposure(risk_acquisition.backlog_units)
            ),
            Theme::signal_flip(),
        ));
    }

    Line::from(spans)
}

fn format_pnl_summary_line(
    pnl_asset: &str,
    total_pnl: Option<f64>,
    unrealized_pnl: f64,
    gross_realized_pnl: f64,
    net_realized_pnl: Option<f64>,
) -> Line<'static> {
    let total = crate::signal::pnl_aggregate_signal(total_pnl);
    let unrealized = pnl_signal(unrealized_pnl);
    let gross_realized = pnl_signal(gross_realized_pnl);
    let net_realized = crate::signal::pnl_aggregate_signal(net_realized_pnl);

    Line::from(vec![
        Span::raw("total "),
        Span::styled(format!("{} {}", total.text, pnl_asset), total.style),
        Span::raw(" | unrealized "),
        Span::styled(
            format!("{} {}", unrealized.text, pnl_asset),
            unrealized.style,
        ),
        Span::raw(" | gross realized "),
        Span::styled(
            format!("{} {}", gross_realized.text, pnl_asset),
            gross_realized.style,
        ),
        Span::raw(" | net realized "),
        Span::styled(
            format!("{} {}", net_realized.text, pnl_asset),
            net_realized.style,
        ),
    ])
}

fn format_pnl_cost_line(
    pnl_asset: &str,
    trading_fee_cumulative: f64,
    funding_fee_cumulative: f64,
) -> Line<'static> {
    let trading_fee = pnl_signal(-trading_fee_cumulative);
    let funding_fee = pnl_signal(funding_fee_cumulative);

    Line::from(vec![
        Span::raw("fee cumulative "),
        Span::styled(
            format!("{} {}", trading_fee.text, pnl_asset),
            trading_fee.style,
        ),
        Span::raw(" | funding cumulative "),
        Span::styled(
            format!("{} {}", funding_fee.text, pnl_asset),
            funding_fee.style,
        ),
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

#[cfg(test)]
mod tests {
    use crate::app::{App, View};
    use crate::protocol::{
        DecisionReasonView, ExecutionStatusView, InventoryDecisionView, TrackCommandType,
        TrackCommandView, TrackDetailView, TrackDiagnosticsView,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{execution_body_line_count, execution_lines, pnl_lines, render};
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

    fn render_app_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, frame.area(), app))
            .unwrap();

        buffer_text(&terminal)
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

    fn detail_with_distinct_decision() -> TrackDetailView {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.decision = InventoryDecisionView {
            revision: Some(77),
            policy_target: Some(6.0),
            risk_approved_target: Some(4.5),
            acquisition_frontier: Some(4.0),
            execution_target: Some(3.75),
            reason: DecisionReasonView::WaitingRiskRelease,
        };
        detail
    }

    #[test]
    fn renders_revision_reason_and_distinct_four_target_chain() {
        let detail = detail_with_distinct_decision();

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 4)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines[0], "decision r77 | reason waiting_risk_release");
        assert_eq!(
            lines[1],
            "targets: policy +6.0000 | approved +4.5000 | frontier +4.0000 | execution +3.7500"
        );
    }

    #[test]
    fn standard_layout_counts_bindings_none_line() {
        let mut detail = detail_with_distinct_decision();
        detail.execution.risk_acquisition = None;
        detail.execution.bindings.clear();
        detail.execution.active_binding_count = 0;

        let body_line_count = execution_body_line_count(&detail);
        let lines = execution_lines(&detail, DetailLayoutMode::Standard, body_line_count)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(body_line_count, 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "bindings: none");
        assert!(!lines.join(" ").contains("more execution lines"));
    }

    #[test]
    fn renders_unavailable_decision_without_target_fallback_or_zero() {
        let mut detail = detail_with_distinct_decision();
        detail.decision = InventoryDecisionView {
            revision: None,
            policy_target: None,
            risk_approved_target: None,
            acquisition_frontier: None,
            execution_target: None,
            reason: DecisionReasonView::WaitingMarketData,
        };

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 4)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines[0], "decision - | reason waiting_market_data");
        assert_eq!(lines[1], "targets unavailable");
        assert!(!lines.join(" ").contains("+0.0000"));
    }

    #[test]
    fn keeps_available_targets_when_only_frontier_is_unavailable() {
        let mut detail = detail_with_distinct_decision();
        detail.decision.acquisition_frontier = None;

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 4)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            lines[1],
            "targets: policy +6.0000 | approved +4.5000 | frontier - | execution +3.7500"
        );
    }

    #[test]
    fn compact_and_minimal_layouts_keep_decision_visible_with_attention() {
        let mut detail = detail_with_distinct_decision();
        detail.execution.execution_status = ExecutionStatusView::AttentionRequired;
        detail.execution.attention_reasons = vec!["recovery anomaly".into()];

        for mode in [DetailLayoutMode::Compact, DetailLayoutMode::Minimal] {
            let lines = execution_lines(&detail, mode, 1)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>();
            assert_eq!(
                lines,
                [
                    "r77 waiting_risk_release | policy +6.00 approved +4.50 frontier +4.00 exec +3.75"
                ]
            );
        }
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
    fn does_not_render_stale_detail_for_a_different_selection() {
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut tracks = response.items;
        let mut other = tracks[0].clone();
        other.id = "eth-core".into();
        tracks.push(other);

        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        let mut app = App::new(tracks);
        app.current_view = View::Instance;
        app.apply_track_detail(detail);
        app.select_next();

        let text = render_app_text(&app, 100, 36);

        assert!(text.contains("No track detail loaded"));
        assert!(!text.contains("commands: p pause"));
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
        assert!(text.contains(
            "prices: strategy 101.2500 (live) | mark 101.3000 | bid 101.2000 | ask 101.4000"
        ));
        assert!(text.contains("position: qty 0.420000 | notional 42.55 USDT"));
        assert!(text.contains("exposure: 3.5000 → 4.0000 [↑ +0.5000]"));
        assert!(text.contains("lower/upper: 90.0000 / 110.0000"));
        assert!(text.contains("units 8.0000/8.0000 | notional 375.0000"));
        assert!(text.contains("maker 1 submit_pending increase_inventory buy 0.0100 @ 100.5000"));
        assert!(!text.contains("replacement gate"));
        assert!(!text.contains("Diagnostics"));
        assert!(!text.contains("client-1"));
    }

    #[test]
    fn hides_risk_acquisition_config_when_backlog_is_absent() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        assert!(detail.execution.risk_acquisition.is_none());
        let text = render_text_with_size(detail, 180, 36);

        assert!(!text.contains("acq: init"));
        assert!(!text.contains("acq: long"));
    }

    #[test]
    fn renders_fresh_acquisition_session_without_backlog() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        value["execution"]["acquisition_session"] = serde_json::json!({
            "acquisition_reset_reason": "fresh_session_start",
            "acquisition_session_started_at": "2026-07-01T08:00:00+00:00",
            "initial_frontier": 2.0,
            "initial_release_units": 0.0
        });
        let detail: TrackDetailView = serde_json::from_value(value).unwrap();
        assert!(detail.execution.risk_acquisition.is_none());

        let lines = execution_lines(
            &detail,
            DetailLayoutMode::Standard,
            execution_body_line_count(&detail),
        )
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();

        assert!(lines.iter().any(|line| {
            line.contains("acq session: fresh_session_start")
                && line.contains("initial frontier +2.0000")
                && line.contains("initial release +0.0000")
        }));
    }

    #[test]
    fn renders_risk_acquisition_backlog_observability() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        value["execution"]["risk_acquisition"] = serde_json::json!({
            "direction": "long",
            "backlog_units": 3.625,
            "release_anchor_price": 100.0,
            "release_anchor_target": 4.0,
            "stale_release_elapsed_minutes": 12.0,
            "stale_release_minutes": 30.0,
            "next_advantage_target": 6.0,
            "next_advantage_price": 92.5,
            "next_release_units": 0.875,
            "next_release_target": 3.25
        });
        value["decision"] = serde_json::json!({
            "revision": 43,
            "policy_target": 6.0,
            "risk_approved_target": 6.0,
            "acquisition_frontier": 2.375,
            "execution_target": 3.75,
            "reason": "waiting_risk_release"
        });
        let detail: TrackDetailView = serde_json::from_value(value).unwrap();

        let text = render_text_with_size(detail, 180, 36);

        assert!(text.contains("exposure: 3.5000 → 3.7500 [↑ +0.2500] [acq backlog +3.6250]"));
        assert!(text.contains("acq: long | backlog +3.6250 | release +0.8750 -> +3.2500"));
        assert!(
            text.contains("unlock: price <= 92.5000 or stale in 18m | anchor 100.0000/+4.0000")
        );
    }

    #[test]
    fn renders_execution_target_state_without_active_bindings() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.execution.bindings.clear();
        detail.execution.active_binding_count = 0;
        detail.decision.execution_target = Some(3.75);

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 4)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines[0], "decision r42 | reason submit_planned");
        assert_eq!(
            lines[1],
            "targets: policy +4.0000 | approved +4.0000 | frontier +4.0000 | execution +3.7500"
        );
        assert!(lines.iter().any(|line| line == "bindings: none"));
        assert!(!lines.iter().any(|line| line == "no active bindings"));
    }

    #[test]
    fn renders_risk_acquisition_without_hiding_no_binding_state() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        value["execution"]["bindings"] = serde_json::json!([]);
        value["execution"]["active_binding_count"] = serde_json::json!(0);
        value["execution"]["risk_acquisition"] = serde_json::json!({
            "direction": "long",
            "backlog_units": 0.3742,
            "release_anchor_price": 64399.5,
            "release_anchor_target": 0.0,
            "stale_release_elapsed_minutes": 0.0,
            "stale_release_minutes": 60.0,
            "next_advantage_target": 0.6,
            "next_advantage_price": 64201.9945,
            "next_release_units": 0.3742,
            "next_release_target": 0.3742
        });
        value["position"]["current_exposure"] = serde_json::json!(-0.1056);
        value["decision"] = serde_json::json!({
            "revision": 43,
            "policy_target": 0.3742,
            "risk_approved_target": 0.3742,
            "acquisition_frontier": 0.0,
            "execution_target": 0.0,
            "reason": "waiting_risk_release"
        });
        let detail: TrackDetailView = serde_json::from_value(value).unwrap();

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 4)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "decision r43 | reason waiting_risk_release");
        assert_eq!(
            lines[1],
            "targets: policy +0.3742 | approved +0.3742 | frontier +0.0000 | execution +0.0000"
        );
        assert_eq!(
            lines[2],
            "acq: long | backlog +0.3742 | release +0.3742 -> +0.3742"
        );
        assert_eq!(
            lines[3],
            "unlock: price <= 64201.9945 or stale in 60m | anchor 64399.5000/+0.0000"
        );
        assert!(!lines[2].contains("frontier"));
        assert!(!lines.iter().any(|line| line == "bindings: none"));
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

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 8)
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

        let lines = execution_lines(&detail, DetailLayoutMode::Standard, 5)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 5);
        assert!(lines[4].contains("+6 more execution lines"));
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
        detail.pnl.total_pnl = Some(-123456789.12);
        detail.pnl.gross_realized_pnl = 987654321.99;
        detail.pnl.net_realized_pnl = Some(876543210.88);
        detail.pnl.unrealized_pnl = -111111111.11;
        detail.pnl.trading_fee_cumulative = 12.34;
        detail.pnl.funding_fee_cumulative = -5.67;

        let pnl_text = pnl_lines(&detail).iter().map(line_text).collect::<Vec<_>>();

        assert!(pnl_text[0].contains("total ↓ -123456789.12"));
        assert!(pnl_text[0].contains("unrealized ↓ -111111111.11"));
        assert!(pnl_text[0].contains("gross realized ↑ +987654321.99"));
        assert!(pnl_text[0].contains("net realized ↑ +876543210.88"));
        assert!(pnl_text[1].contains("fee cumulative ↓ -12.34"));
        assert!(pnl_text[1].contains("funding cumulative ↓ -5.67"));
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
