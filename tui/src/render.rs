use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Tabs, Wrap},
};

use crate::{
    locale,
    protocol::OpenOrdersSource,
    selectors::{self, ConnectionHealthKind},
    state::{AppState, CommandTimelineStage, Modal, Page, SnapshotBootstrapState, ToastLevel},
    theme::{PanelTone, StatusTone, Theme},
};

pub fn draw(frame: &mut Frame<'_>, state: &AppState, theme: &Theme) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_base)),
        frame.area(),
    );

    let viewport = Viewport::new(frame.area());
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(viewport.status_height()),
            Constraint::Length(viewport.tabs_height()),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(frame.area());

    draw_status(frame, outer[0], state, theme, viewport);
    draw_tabs(frame, outer[1], state, theme);
    draw_main(frame, outer[2], state, theme, viewport);
    draw_footer(frame, outer[3], state, theme, viewport);

    if let Some(modal) = &state.ui.modal {
        draw_modal(frame, modal, state.ui.locale, theme);
    }
}

#[derive(Debug, Clone, Copy)]
struct Viewport {
    width: u16,
    height: u16,
}

impl Viewport {
    fn new(area: Rect) -> Self {
        Self {
            width: area.width,
            height: area.height,
        }
    }

    fn status_height(self) -> u16 {
        if self.height < 20 { 1 } else { 2 }
    }

    fn tabs_height(self) -> u16 {
        if self.height < 18 { 2 } else { 3 }
    }

    fn compact(self) -> bool {
        self.width < 96
    }

    fn narrow(self) -> bool {
        self.width < 112
    }
}

fn draw_status(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let focus = state
        .ui
        .page
        .focus_label(state.ui.locale, state.ui.focus_index);
    let mut spans = vec![Span::styled(" GRID PLATFORM ", theme.emphasis())];

    match &state.snapshot_state {
        SnapshotBootstrapState::WaitingFirstSnapshot => {
            spans.push(badge_span(
                copy.status().waiting_snapshot_badge(),
                theme,
                StatusTone::Warning,
            ));
            spans.push(Span::styled(
                copy.status().waiting_snapshot_message(viewport.narrow()),
                theme.muted(),
            ));
        }
        SnapshotBootstrapState::SnapshotRetrying {
            last_error: _,
            retry_count,
            retry_in_ms,
        } => {
            spans.push(badge_span(
                copy.status().snapshot_failed_badge(),
                theme,
                StatusTone::Danger,
            ));
            spans.push(Span::styled(
                copy.status().retry_status(*retry_count, *retry_in_ms),
                theme.danger(),
            ));
        }
        SnapshotBootstrapState::Ready => {
            let vm = selectors::dashboard(state);
            let health = selectors::connection_health(state);
            spans.push(Span::styled(
                format!(
                    " {} {} {} ",
                    vm.symbol, state.runtime.env, vm.strategy_state
                ),
                theme.panel().add_modifier(Modifier::BOLD),
            ));
            spans.push(badge_span(
                health.label,
                theme,
                status_tone_for_connection(health.kind),
            ));
            if viewport.narrow() {
                spans.push(Span::styled(
                    copy.status().focus_status(focus),
                    theme.info(),
                ));
            } else {
                spans.push(Span::styled(format!(" {} ", health.detail), theme.muted()));
                spans.push(Span::styled(
                    copy.status().focus_status(focus),
                    theme.info(),
                ));
                spans.push(Span::styled(
                    copy.status()
                        .pending_status(state.execution.pending_commands.len()),
                    theme.warning(),
                ));
            }
        }
    }

    if area.height == 1 {
        frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.panel()), area);
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme.panel())
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(theme.border_idle)),
            ),
        area,
    );
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let titles = locale::copy(state.ui.locale)
        .tabs()
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    let selected = match state.ui.page {
        Page::Dashboard => 0,
        Page::Grid => 1,
        Page::Market => 2,
        Page::Events => 3,
        Page::Help => 4,
    };
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(theme.muted())
            .highlight_style(theme.emphasis())
            .divider(" "),
        area,
    );
}

fn draw_main(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    if !state.is_snapshot_ready() && !matches!(state.ui.page, Page::Help) {
        draw_bootstrap_main(frame, area, state, theme, viewport);
        return;
    }

    match state.ui.page {
        Page::Dashboard => draw_dashboard(frame, area, state, theme, viewport),
        Page::Grid => draw_grid(frame, area, state, theme, viewport),
        Page::Market => draw_market(frame, area, state, theme, viewport),
        Page::Events => draw_events(frame, area, state, theme, viewport),
        Page::Help => draw_help(frame, area, state, theme, viewport),
    }
}

fn draw_bootstrap_main(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    match state.ui.page {
        Page::Dashboard => draw_bootstrap_dashboard(frame, area, state, theme, viewport),
        Page::Grid => draw_bootstrap_grid(frame, area, state, theme, viewport),
        Page::Market => draw_bootstrap_market(frame, area, state, theme, viewport),
        Page::Events => draw_bootstrap_events(frame, area, state, theme, viewport),
        Page::Help => draw_help(frame, area, state, theme, viewport),
    }
}

fn draw_bootstrap_dashboard(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if viewport.narrow() {
            [Constraint::Percentage(56), Constraint::Percentage(44)]
        } else {
            [Constraint::Percentage(62), Constraint::Percentage(38)]
        })
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints(dashboard_left_constraints(area.height))
        .split(split[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(dashboard_right_constraints(area.height))
        .split(split[1]);

    bootstrap_block(
        frame,
        left[0],
        theme,
        copy.dashboard().execution_focus_title(),
        panel_focused(state, 0),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        left[1],
        theme,
        copy.dashboard().open_orders_title(),
        panel_focused(state, 1),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        left[2],
        theme,
        copy.dashboard().recent_fills_title(),
        panel_focused(state, 2),
        PanelTone::Success,
        state,
    );
    bootstrap_block(
        frame,
        right[0],
        theme,
        copy.dashboard().risk_alerts_title(),
        panel_focused(state, 3),
        PanelTone::Warning,
        state,
    );
    bootstrap_block(
        frame,
        right[1],
        theme,
        copy.dashboard().market_health_title(),
        panel_focused(state, 4),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        right[2],
        theme,
        copy.dashboard().command_timeline_title(),
        panel_focused(state, 5),
        PanelTone::Neutral,
        state,
    );
}

fn draw_bootstrap_grid(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    if viewport.compact() {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(6),
                Constraint::Length(5),
                Constraint::Min(4),
            ])
            .split(area);
        bootstrap_block(
            frame,
            layout[0],
            theme,
            copy.grid().active_grid_levels_title(),
            panel_focused(state, 0),
            PanelTone::Neutral,
            state,
        );
        bootstrap_block(
            frame,
            layout[1],
            theme,
            copy.grid().grid_summary_title(),
            panel_focused(state, 1),
            PanelTone::Neutral,
            state,
        );
        bootstrap_block(
            frame,
            layout[2],
            theme,
            copy.grid().operator_notes_title(),
            panel_focused(state, 2),
            PanelTone::Neutral,
            state,
        );
    } else {
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(area);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(5)])
            .split(layout[1]);
        bootstrap_block(
            frame,
            layout[0],
            theme,
            copy.grid().active_grid_levels_title(),
            panel_focused(state, 0),
            PanelTone::Neutral,
            state,
        );
        bootstrap_block(
            frame,
            right[0],
            theme,
            copy.grid().grid_summary_title(),
            panel_focused(state, 1),
            PanelTone::Neutral,
            state,
        );
        bootstrap_block(
            frame,
            right[1],
            theme,
            copy.grid().operator_notes_title(),
            panel_focused(state, 2),
            PanelTone::Neutral,
            state,
        );
    }
}

fn draw_bootstrap_market(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let layout = if viewport.compact() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Min(4),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(area)
    };

    bootstrap_block(
        frame,
        layout[0],
        theme,
        copy.market().tape_title(),
        panel_focused(state, 0),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        layout[1],
        theme,
        copy.market().connectivity_title(),
        panel_focused(state, 1),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        layout[2],
        theme,
        copy.market().runtime_title(),
        panel_focused(state, 2),
        PanelTone::Neutral,
        state,
    );
}

fn draw_bootstrap_events(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    _viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    bootstrap_block(
        frame,
        top[0],
        theme,
        copy.events().fills_panel_title(),
        panel_focused(state, 0),
        PanelTone::Success,
        state,
    );
    bootstrap_block(
        frame,
        top[1],
        theme,
        copy.events().alerts_panel_title(),
        panel_focused(state, 1),
        PanelTone::Warning,
        state,
    );
    bootstrap_block(
        frame,
        bottom[0],
        theme,
        copy.events().commands_panel_title(),
        panel_focused(state, 2),
        PanelTone::Neutral,
        state,
    );
    bootstrap_block(
        frame,
        bottom[1],
        theme,
        copy.events().system_panel_title(),
        panel_focused(state, 3),
        PanelTone::Neutral,
        state,
    );
}

fn bootstrap_block(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    title: &str,
    focused: bool,
    tone: PanelTone,
    state: &AppState,
) {
    let body = bootstrap_lines(state, theme, title);
    frame.render_widget(
        Paragraph::new(body)
            .block(panel_block(theme, title, focused, tone))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn bootstrap_lines(state: &AppState, theme: &Theme, title: &str) -> Vec<Line<'static>> {
    let copy = locale::copy(state.ui.locale);
    match &state.snapshot_state {
        SnapshotBootstrapState::WaitingFirstSnapshot => vec![
            Line::from(vec![Span::styled(
                copy.bootstrap().pending_title(),
                theme.warning(),
            )]),
            Line::from(copy.bootstrap().pending_detail()),
            Line::from(copy.bootstrap().pending_actions_disabled()),
            Line::from(copy.bootstrap().panel_line(title)),
        ],
        SnapshotBootstrapState::SnapshotRetrying {
            last_error,
            retry_count,
            retry_in_ms,
        } => vec![
            Line::from(vec![Span::styled(
                copy.bootstrap().failed_title(),
                theme.danger(),
            )]),
            Line::from(copy.bootstrap().failed_retry(*retry_count, *retry_in_ms)),
            Line::from(copy.bootstrap().error_line(last_error)),
            Line::from(copy.bootstrap().failed_actions_disabled()),
            Line::from(copy.bootstrap().panel_line(title)),
        ],
        SnapshotBootstrapState::Ready => Vec::new(),
    }
}

fn draw_dashboard(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let dashboard_copy = copy.dashboard();
    let common = copy.common();
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if viewport.narrow() {
            [Constraint::Percentage(56), Constraint::Percentage(44)]
        } else {
            [Constraint::Percentage(62), Constraint::Percentage(38)]
        })
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints(dashboard_left_constraints(area.height))
        .split(split[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(dashboard_right_constraints(area.height))
        .split(split[1]);

    let vm = selectors::dashboard(state);
    let health = selectors::connection_health(state);
    let exchange_orders = vm
        .exchange_orders
        .map(|count| count.to_string())
        .unwrap_or_else(|| common.not_available().into());
    let summary_lines = if left[0].height <= 4 {
        vec![
            Line::from(vec![
                Span::styled(dashboard_copy.pos_short_label(), theme.muted()),
                Span::styled(vm.position_qty, theme.emphasis()),
                Span::styled(" @ ", theme.muted()),
                Span::styled(vm.position_avg_price, theme.panel()),
                Span::styled(dashboard_copy.unrealized_short_label(), theme.muted()),
                Span::styled(
                    vm.unrealized_pnl.clone(),
                    if state.runtime.unrealized_pnl >= 0.0 {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
                Span::styled(dashboard_copy.realized_short_label(), theme.muted()),
                Span::styled(
                    vm.realized_pnl.clone(),
                    if state.runtime.realized_pnl >= 0.0 {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.exchange_label(), theme.muted()),
                Span::styled(
                    exchange_orders,
                    if vm.exchange_orders.is_some() {
                        theme.panel()
                    } else {
                        theme.warning()
                    },
                ),
                Span::styled(dashboard_copy.pending_label(), theme.muted()),
                Span::styled(vm.pending_commands.to_string(), theme.warning()),
                Span::styled(dashboard_copy.health_label(), theme.muted()),
                Span::styled(
                    health.label,
                    theme.status(status_tone_for_connection(health.kind)),
                ),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(dashboard_copy.position_label(), theme.muted()),
                Span::styled(vm.position_qty, theme.emphasis()),
                Span::styled(" @ ", theme.muted()),
                Span::styled(vm.position_avg_price, theme.panel()),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.unrealized_label(), theme.muted()),
                Span::styled(
                    vm.unrealized_pnl.clone(),
                    if state.runtime.unrealized_pnl >= 0.0 {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
                Span::styled(dashboard_copy.realized_label(), theme.muted()),
                Span::styled(
                    vm.realized_pnl.clone(),
                    if state.runtime.realized_pnl >= 0.0 {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.exchange_label(), theme.muted()),
                Span::styled(
                    exchange_orders,
                    if vm.exchange_orders.is_some() {
                        theme.panel()
                    } else {
                        theme.warning()
                    },
                ),
                Span::styled(dashboard_copy.pending_label(), theme.muted()),
                Span::styled(vm.pending_commands.to_string(), theme.warning()),
                Span::styled(dashboard_copy.health_label(), theme.muted()),
                Span::styled(
                    health.label,
                    theme.status(status_tone_for_connection(health.kind)),
                ),
            ]),
        ]
    };
    let summary = Paragraph::new(summary_lines)
        .block(panel_block(
            theme,
            dashboard_copy.execution_focus_title(),
            panel_focused(state, 0),
            PanelTone::Neutral,
        ))
        .wrap(Wrap { trim: true });
    frame.render_widget(summary, left[0]);

    match state.execution.exchange_open_orders_source {
        OpenOrdersSource::ExchangeLive => {
            let order_rows = selectors::open_order_items(state, table_capacity(left[1]))
                .into_iter()
                .map(|order| {
                    Row::new(vec![
                        Cell::from(order.side),
                        Cell::from(order.price),
                        Cell::from(order.qty),
                        Cell::from(match order.command_ref {
                            Some(command_ref) => format!("{} · {}", order.status, command_ref),
                            None => order.status,
                        }),
                    ])
                });
            frame.render_widget(
                Table::new(
                    order_rows,
                    [
                        Constraint::Length(6),
                        Constraint::Length(9),
                        Constraint::Length(8),
                        Constraint::Min(7),
                    ],
                )
                .header(
                    Row::new(vec![
                        dashboard_copy.side_header(),
                        dashboard_copy.price_header(),
                        dashboard_copy.qty_header(),
                        dashboard_copy.status_header(),
                    ])
                    .style(theme.emphasis()),
                )
                .column_spacing(1)
                .block(panel_block(
                    theme,
                    dashboard_copy.exchange_orders_title(),
                    panel_focused(state, 1),
                    PanelTone::Neutral,
                )),
                left[1],
            );
        }
        OpenOrdersSource::StrategyMirror | OpenOrdersSource::Unavailable => {
            frame.render_widget(
                Paragraph::new(exchange_orders_unavailable_lines(state, theme))
                    .block(panel_block(
                        theme,
                        dashboard_copy.exchange_orders_title(),
                        panel_focused(state, 1),
                        PanelTone::Warning,
                    ))
                    .wrap(Wrap { trim: true }),
                left[1],
            );
        }
    }

    let fill_items = list_from_lines(
        selectors::recent_fill_items(state, list_capacity(left[2]))
            .into_iter()
            .flat_map(|fill| {
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{} ", fill.side), theme.emphasis()),
                    Span::styled(fill.price_qty, theme.panel()),
                    Span::styled(
                        dashboard_copy.pnl_label(&fill.pnl),
                        if fill.realized_pnl >= 0.0 {
                            theme.success()
                        } else {
                            theme.danger()
                        },
                    ),
                ])];
                if let Some(command_ref) = fill.command_ref {
                    lines.push(Line::from(Span::styled(
                        dashboard_copy.command_ref_label(&command_ref),
                        theme.muted(),
                    )));
                }
                lines
            })
            .collect(),
        theme,
        dashboard_copy.no_fills(),
    );
    frame.render_widget(
        List::new(fill_items).block(panel_block(
            theme,
            dashboard_copy.recent_fills_title(),
            panel_focused(state, 2),
            PanelTone::Success,
        )),
        left[2],
    );

    let top_alert = state.risk.alerts.front();
    let risk = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(dashboard_copy.level_label(), theme.muted()),
            Span::styled(
                dashboard_copy.risk_level_label(state.risk.risk_level),
                risk_style(theme, state),
            ),
            Span::styled(dashboard_copy.breaker_label(), theme.muted()),
            Span::styled(
                if state.risk.breaker_engaged {
                    common.on()
                } else {
                    common.off()
                },
                if state.risk.breaker_engaged {
                    theme.danger()
                } else {
                    theme.success()
                },
            ),
        ]),
        Line::from(vec![
            Span::styled(dashboard_copy.notional_label(), theme.muted()),
            Span::styled(
                format!(
                    "{:.0}/{:.0}",
                    state.risk.current_notional, state.risk.max_notional
                ),
                theme.panel().add_modifier(Modifier::BOLD),
            ),
            Span::styled(dashboard_copy.stop_label(), theme.muted()),
            Span::styled(format!("{:.1}%", state.risk.stop_loss_pct), theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(dashboard_copy.alert_label(), theme.muted()),
            Span::styled(
                top_alert
                    .map(|alert| alert.code.clone())
                    .unwrap_or_else(|| common.none_value().into()),
                if top_alert.is_some() {
                    theme.warning()
                } else {
                    theme.muted()
                },
            ),
        ]),
        Line::from(
            top_alert
                .map(|alert| alert.message.clone())
                .unwrap_or_else(|| dashboard_copy.no_active_alerts().into()),
        ),
    ])
    .block(panel_block(
        theme,
        dashboard_copy.risk_alerts_title(),
        panel_focused(state, 3),
        risk_panel_tone(state),
    ))
    .wrap(Wrap { trim: true });
    frame.render_widget(risk, right[0]);

    let health_detail = selectors::dashboard_health_detail(state);
    let market_lines = if right[1].height <= 4 {
        vec![
            Line::from(vec![
                Span::styled(dashboard_copy.last_label(), theme.muted()),
                Span::styled(format!("{:.2}", state.runtime.last_price), theme.emphasis()),
                Span::styled(dashboard_copy.mark_label(), theme.muted()),
                Span::styled(format!("{:.2}", state.runtime.mark_price), theme.panel()),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.service_ws_short_label(), theme.muted()),
                Span::styled(
                    if state.connection.ws_connected {
                        common.up()
                    } else {
                        common.down()
                    },
                    if state.connection.ws_connected {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
                Span::styled(dashboard_copy.market_ws_short_label(), theme.muted()),
                Span::styled(
                    if state.connection.market_ws_connected {
                        common.up()
                    } else {
                        common.down()
                    },
                    if state.connection.market_ws_connected {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
                Span::styled("   ", theme.muted()),
                Span::styled(health_detail, theme.muted()),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(dashboard_copy.last_label(), theme.muted()),
                Span::styled(format!("{:.2}", state.runtime.last_price), theme.emphasis()),
                Span::styled(dashboard_copy.mark_label(), theme.muted()),
                Span::styled(format!("{:.2}", state.runtime.mark_price), theme.panel()),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.service_ws_label(), theme.muted()),
                Span::styled(
                    if state.connection.ws_connected {
                        common.up()
                    } else {
                        common.down()
                    },
                    if state.connection.ws_connected {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
                Span::styled(dashboard_copy.market_ws_label(), theme.muted()),
                Span::styled(
                    if state.connection.market_ws_connected {
                        common.up()
                    } else {
                        common.down()
                    },
                    if state.connection.market_ws_connected {
                        theme.success()
                    } else {
                        theme.danger()
                    },
                ),
            ]),
            Line::from(vec![
                Span::styled(dashboard_copy.health_line_label(), theme.muted()),
                Span::styled(
                    health.label,
                    theme.status(status_tone_for_connection(health.kind)),
                ),
                Span::styled("   ", theme.muted()),
                Span::styled(health_detail, theme.muted()),
            ]),
        ]
    };
    let market = Paragraph::new(market_lines)
        .block(panel_block(
            theme,
            dashboard_copy.market_health_title(),
            panel_focused(state, 4),
            panel_tone_for_connection(health.kind),
        ))
        .wrap(Wrap { trim: true });
    frame.render_widget(market, right[1]);

    let command_items = command_timeline_items(state, theme, right[2]);
    frame.render_widget(
        List::new(command_items).block(panel_block(
            theme,
            dashboard_copy.command_timeline_title(),
            panel_focused(state, 5),
            command_panel_tone(state),
        )),
        right[2],
    );
}

fn draw_grid(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let vm = selectors::grid(state);
    if viewport.compact() {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(6),
                Constraint::Length(5),
                Constraint::Min(4),
            ])
            .split(area);

        draw_strategy_orders(frame, layout[0], state, theme);
        draw_grid_summary(frame, layout[1], state, theme, &vm);
        draw_grid_notes(frame, layout[2], state, theme);
    } else {
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(area);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(5)])
            .split(layout[1]);

        draw_strategy_orders(frame, layout[0], state, theme);
        draw_grid_summary(frame, right[0], state, theme, &vm);
        draw_grid_notes(frame, right[1], state, theme);
    }
}

fn draw_strategy_orders(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let copy = locale::copy(state.ui.locale);
    let order_rows = selectors::strategy_orders(state)
        .into_iter()
        .take(table_capacity(area))
        .map(|order| {
            Row::new(vec![
                Cell::from(order.side),
                Cell::from(order.price),
                Cell::from(order.qty),
                Cell::from(strategy_state_label(state, &order.strategy_state)),
                Cell::from(placement_state_label(state, order.placement_state)),
            ])
        });
    frame.render_widget(
        Table::new(
            order_rows,
            [
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(vec![
                copy.dashboard().side_header(),
                copy.dashboard().price_header(),
                copy.dashboard().qty_header(),
                copy.grid().strategy_header(),
                copy.grid().placement_header(),
            ])
            .style(theme.emphasis()),
        )
        .column_spacing(1)
        .block(panel_block(
            theme,
            copy.grid().strategy_orders_title(),
            panel_focused(state, 0),
            PanelTone::Neutral,
        )),
        area,
    );
}

fn draw_grid_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    vm: &selectors::GridViewModel,
) {
    let copy = locale::copy(state.ui.locale);
    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(copy.grid().status_label(), theme.muted()),
            Span::styled(vm.status.clone(), theme.emphasis()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().lower_label(), theme.muted()),
            Span::styled(vm.lower.clone(), theme.panel()),
            Span::styled(copy.grid().upper_label(), theme.muted()),
            Span::styled(vm.upper.clone(), theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().center_label(), theme.muted()),
            Span::styled(vm.center.clone(), theme.emphasis()),
            Span::styled(copy.grid().span_label(), theme.muted()),
            Span::styled(vm.span_pct.clone(), theme.warning()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().active_label(), theme.muted()),
            Span::styled(vm.active_levels.to_string(), theme.panel()),
            Span::styled(copy.grid().occupied_label(), theme.muted()),
            Span::styled(vm.occupied_levels.to_string(), theme.warning()),
            Span::styled(copy.grid().pending_label(), theme.muted()),
            Span::styled(vm.pending_levels.to_string(), theme.warning()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().bias_label(), theme.muted()),
            Span::styled(vm.inventory_bias.clone(), theme.warning()),
        ]),
    ])
    .block(panel_block(
        theme,
        copy.grid().grid_summary_title(),
        panel_focused(state, 1),
        PanelTone::Neutral,
    ))
    .wrap(Wrap { trim: true });
    frame.render_widget(summary, area);
}

fn draw_grid_notes(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let copy = locale::copy(state.ui.locale);
    let common = copy.common();
    let health = selectors::connection_health(state);
    let notes = vec![
        Line::from(vec![
            Span::styled(copy.grid().current_price_label(), theme.muted()),
            Span::styled(format!("{:.2}", state.runtime.last_price), theme.emphasis()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().session_label(), theme.muted()),
            Span::styled(state.runtime.session_state.clone(), theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().health_label(), theme.muted()),
            Span::styled(
                health.label,
                theme.status(status_tone_for_connection(health.kind)),
            ),
        ]),
        Line::from(vec![
            Span::styled(copy.grid().breaker_label(), theme.muted()),
            Span::styled(
                if state.risk.breaker_engaged {
                    common.on()
                } else {
                    common.off()
                },
                if state.risk.breaker_engaged {
                    theme.danger()
                } else {
                    theme.success()
                },
            ),
        ]),
        Line::from(
            state
                .strategy
                .status_reason
                .clone()
                .unwrap_or_else(|| copy.grid().aligned_message().into()),
        ),
    ];
    frame.render_widget(
        Paragraph::new(notes)
            .block(panel_block(
                theme,
                copy.grid().operator_notes_title(),
                panel_focused(state, 2),
                panel_tone_for_connection(health.kind),
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_market(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let vm = selectors::market(state);
    let health = selectors::connection_health(state);
    let layout = if viewport.compact() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Min(4),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(area)
    };

    let price = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(copy.market().last_label(), theme.muted()),
            Span::styled(vm.last_price, theme.emphasis()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().mark_label(), theme.muted()),
            Span::styled(vm.mark_price, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().basis_label(), theme.muted()),
            Span::styled(
                vm.basis,
                if state.runtime.mark_price >= state.runtime.last_price {
                    theme.warning()
                } else {
                    theme.success()
                },
            ),
        ]),
    ])
    .block(panel_block(
        theme,
        copy.market().tape_title(),
        panel_focused(state, 0),
        PanelTone::Neutral,
    ));
    frame.render_widget(price, layout[0]);

    let connectivity = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(copy.market().service_ws_label(), theme.muted()),
            Span::styled(vm.service_ws_status, theme.panel()),
            Span::styled(copy.market().http_label(), theme.muted()),
            Span::styled(vm.http_status, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().market_ws_label(), theme.muted()),
            Span::styled(vm.market_ws_status, theme.panel()),
            Span::styled(copy.market().user_ws_label(), theme.muted()),
            Span::styled(vm.user_stream_status, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().stale_label(), theme.muted()),
            Span::styled(vm.stale_age, theme.warning()),
            Span::styled(copy.market().retry_label(), theme.muted()),
            Span::styled(vm.reconnect_attempt, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().market_backoff_label(), theme.muted()),
            Span::styled(vm.market_backoff, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().mode_label(), theme.muted()),
            Span::styled(
                health.label,
                theme.status(status_tone_for_connection(health.kind)),
            ),
        ]),
        Line::from(health.hint),
    ])
    .block(panel_block(
        theme,
        copy.market().connectivity_title(),
        panel_focused(state, 1),
        panel_tone_for_connection(health.kind),
    ))
    .wrap(Wrap { trim: true });
    frame.render_widget(connectivity, layout[1]);

    let runtime = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(copy.market().session_label(), theme.muted()),
            Span::styled(vm.session_state, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().heartbeat_label(), theme.muted()),
            Span::styled(vm.heartbeat, theme.panel()),
        ]),
        Line::from(vec![
            Span::styled(copy.market().strategy_label(), theme.muted()),
            Span::styled(state.runtime.strategy_state.clone(), theme.warning()),
        ]),
    ])
    .block(panel_block(
        theme,
        copy.market().runtime_title(),
        panel_focused(state, 2),
        PanelTone::Neutral,
    ))
    .wrap(Wrap { trim: true });
    frame.render_widget(runtime, layout[2]);
}

fn draw_events(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    _viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let vm = selectors::events(state);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let fills = list_from_lines(
        selectors::recent_fill_items(state, list_capacity(top[0]))
            .into_iter()
            .flat_map(|fill| {
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{} ", fill.side), theme.emphasis()),
                    Span::styled(fill.price_qty, theme.panel()),
                ])];
                if let Some(command_ref) = fill.command_ref {
                    lines.push(Line::from(Span::styled(
                        copy.events().command_ref_label(&command_ref),
                        theme.muted(),
                    )));
                }
                lines
            })
            .collect(),
        theme,
        copy.events().no_fills(),
    );
    frame.render_widget(
        List::new(fills).block(panel_block(
            theme,
            &copy.events().fills_title(vm.fills_count),
            panel_focused(state, 0),
            PanelTone::Success,
        )),
        top[0],
    );

    let alerts = list_from_lines(
        state
            .risk
            .alerts
            .iter()
            .take(list_capacity(top[1]))
            .flat_map(|alert| {
                vec![
                    Line::from(vec![
                        Span::styled(format!("{} ", alert.code), theme.warning()),
                        Span::styled(alert.message.clone(), theme.panel()),
                    ]),
                    Line::from(Span::styled(
                        selectors::risk_action_hint(state.ui.locale, &alert.code),
                        theme.muted(),
                    )),
                ]
            })
            .collect(),
        theme,
        copy.events().no_alerts(),
    );
    frame.render_widget(
        List::new(alerts).block(panel_block(
            theme,
            &copy.events().alerts_title(vm.alerts_count),
            panel_focused(state, 1),
            risk_panel_tone(state),
        )),
        top[1],
    );

    let commands = command_timeline_items(state, theme, bottom[0]);
    frame.render_widget(
        List::new(commands).block(panel_block(
            theme,
            &copy.events().commands_title(vm.timeline_count),
            panel_focused(state, 2),
            command_panel_tone(state),
        )),
        bottom[0],
    );

    let system = list_from_lines(
        state
            .system_events
            .iter()
            .take(list_capacity(bottom[1]))
            .map(|item| {
                Line::from(vec![
                    Span::styled(format!("[{}] ", item.level), theme.warning()),
                    Span::styled(item.message.clone(), theme.panel()),
                ])
            })
            .collect(),
        theme,
        copy.events().no_system_events(),
    );
    frame.render_widget(
        List::new(system).block(panel_block(
            theme,
            &copy
                .events()
                .system_title(vm.system_count, vm.pending_commands),
            panel_focused(state, 3),
            PanelTone::Neutral,
        )),
        bottom[1],
    );
}

fn draw_help(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let layout = if viewport.compact() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area)
    };
    let health = selectors::connection_health(state);

    frame.render_widget(
        Paragraph::new(
            copy.help()
                .shortcuts_lines()
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .block(panel_block(
            theme,
            copy.help().shortcuts_title(),
            panel_focused(state, 0),
            PanelTone::Neutral,
        ))
        .wrap(Wrap { trim: true }),
        layout[0],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(copy.help().focus_label(), theme.muted()),
                Span::styled(
                    state
                        .ui
                        .page
                        .focus_label(state.ui.locale, state.ui.focus_index),
                    theme.emphasis(),
                ),
            ]),
            Line::from(vec![
                Span::styled(copy.help().health_label(), theme.muted()),
                Span::styled(
                    health.label,
                    theme.status(status_tone_for_connection(health.kind)),
                ),
            ]),
            Line::from(copy.help().glossary_lines()[0]),
            Line::from(copy.help().glossary_lines()[1]),
            Line::from(copy.help().glossary_lines()[2]),
            Line::from(copy.help().glossary_lines()[3]),
        ])
        .block(panel_block(
            theme,
            copy.help().glossary_title(),
            panel_focused(state, 1),
            panel_tone_for_connection(health.kind),
        ))
        .wrap(Wrap { trim: true }),
        layout[1],
    );
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    viewport: Viewport,
) {
    let copy = locale::copy(state.ui.locale);
    let toast_line = if let Some(toast) = &state.ui.toast {
        let style = match toast.level {
            ToastLevel::Info => theme.info(),
            ToastLevel::Warning => theme.warning(),
            ToastLevel::Danger => theme.danger(),
        };
        Line::from(vec![Span::styled(format!(" {} ", toast.message), style)])
    } else if !state.is_snapshot_ready() {
        match &state.snapshot_state {
            SnapshotBootstrapState::WaitingFirstSnapshot => Line::from(vec![Span::styled(
                copy.footer().snapshot_pending(viewport.narrow()),
                theme.muted(),
            )]),
            SnapshotBootstrapState::SnapshotRetrying { retry_in_ms, .. } => {
                Line::from(vec![Span::styled(
                    copy.footer()
                        .snapshot_failed(viewport.narrow(), *retry_in_ms),
                    theme.muted(),
                )])
            }
            SnapshotBootstrapState::Ready => unreachable!(),
        }
    } else if viewport.narrow() {
        Line::from(vec![Span::styled(
            copy.footer().ready(
                true,
                state
                    .ui
                    .page
                    .focus_label(state.ui.locale, state.ui.focus_index),
            ),
            theme.muted(),
        )])
    } else {
        Line::from(vec![Span::styled(
            copy.footer().ready(
                false,
                state
                    .ui
                    .page
                    .focus_label(state.ui.locale, state.ui.focus_index),
            ),
            theme.muted(),
        )])
    };
    frame.render_widget(
        Paragraph::new(toast_line).style(theme.panel()).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme.border_idle)),
        ),
        area,
    );
}

fn draw_modal(
    frame: &mut Frame<'_>,
    modal: &Modal,
    app_locale: crate::locale::Locale,
    theme: &Theme,
) {
    let area = centered_rect(
        frame.area().width.saturating_sub(8).clamp(48, 76),
        if frame.area().height < 20 { 9 } else { 10 },
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let (title, lines) = danger_copy(app_locale, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(panel_block(theme, title, true, PanelTone::Danger))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn danger_copy(
    app_locale: crate::locale::Locale,
    modal: &Modal,
) -> (&'static str, Vec<Line<'static>>) {
    let copy = locale::copy(app_locale);
    match modal {
        Modal::Confirm(command) => {
            let (title, body, detail) = copy.modal().confirm(*command);
            (
                title,
                vec![
                    Line::from(Span::raw(body)),
                    Line::from(Span::styled(
                        detail,
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(copy.modal().confirm_hint()),
                ],
            )
        }
    }
}

fn panel_block(theme: &Theme, title: &str, focused: bool, tone: PanelTone) -> Block<'static> {
    Block::default()
        .title(Line::from(vec![Span::styled(
            format!(" {} ", title),
            theme.panel_title(tone, focused),
        )]))
        .borders(Borders::ALL)
        .border_style(theme.panel_border(tone, focused))
        .style(theme.panel())
}

fn badge_span(label: &str, theme: &Theme, tone: StatusTone) -> Span<'static> {
    Span::styled(format!(" {} ", label), theme.badge(tone))
}

fn exchange_orders_unavailable_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let copy = locale::copy(state.ui.locale);
    if state.execution.exchange_open_orders_source == OpenOrdersSource::ExchangeLive {
        Vec::new()
    } else if state.execution.open_orders_source == OpenOrdersSource::StrategyMirror {
        let lines = copy.common().exchange_orders_mirror_lines();
        vec![
            Line::from(Span::styled(lines[0], theme.warning())),
            Line::from(lines[1]),
            Line::from(Span::styled(lines[2], theme.muted())),
        ]
    } else {
        let lines = copy.common().exchange_orders_unavailable_lines();
        vec![
            Line::from(Span::styled(lines[0], theme.warning())),
            Line::from(Span::styled(lines[1], theme.muted())),
        ]
    }
}

fn strategy_state_label(app_state: &AppState, state: &str) -> String {
    locale::copy(app_state.ui.locale)
        .common()
        .strategy_state_label(state)
}

fn placement_state_label(app_state: &AppState, state: selectors::PlacementState) -> &'static str {
    locale::copy(app_state.ui.locale)
        .common()
        .placement_state_label(state)
}

fn list_from_lines(
    lines: Vec<Line<'static>>,
    theme: &Theme,
    empty_message: &str,
) -> Vec<ListItem<'static>> {
    if lines.is_empty() {
        vec![ListItem::new(Line::from(vec![Span::styled(
            empty_message.to_string(),
            theme.muted(),
        )]))]
    } else {
        lines.into_iter().map(ListItem::new).collect()
    }
}

fn command_timeline_items(state: &AppState, theme: &Theme, area: Rect) -> Vec<ListItem<'static>> {
    let copy = locale::copy(state.ui.locale);
    let compact = area.height < 6;
    let limit = if compact {
        list_capacity(area)
    } else {
        command_capacity(area)
    };
    let items = selectors::command_timeline(state, limit);
    if items.is_empty() {
        return vec![ListItem::new(Line::from(vec![Span::styled(
            copy.common().no_recent_commands().to_string(),
            theme.muted(),
        )]))];
    }

    items
        .into_iter()
        .map(|item| {
            if compact {
                ListItem::new(Line::from(vec![
                    badge_span(item.stage_label, theme, stage_tone(item.stage)),
                    Span::styled(format!(" {} ", item.command_label), theme.emphasis()),
                ]))
            } else {
                ListItem::new(vec![
                    Line::from(vec![
                        badge_span(item.stage_label, theme, stage_tone(item.stage)),
                        Span::styled(format!(" {} ", item.command_label), theme.emphasis()),
                        Span::styled(item.command_id, theme.muted()),
                    ]),
                    Line::from(item.summary),
                    Line::from(Span::styled(item.timing, theme.muted())),
                ])
            }
        })
        .collect()
}

fn dashboard_left_constraints(height: u16) -> [Constraint; 3] {
    if height < 12 {
        [
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(3),
        ]
    } else {
        [
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Min(4),
        ]
    }
}

fn dashboard_right_constraints(height: u16) -> [Constraint; 3] {
    if height < 12 {
        [
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(3),
        ]
    } else {
        [
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Min(4),
        ]
    }
}

fn list_capacity(area: Rect) -> usize {
    area.height.saturating_sub(2).max(1) as usize
}

fn table_capacity(area: Rect) -> usize {
    area.height.saturating_sub(3).max(1) as usize
}

fn command_capacity(area: Rect) -> usize {
    (area.height.saturating_sub(2) / 3).max(1) as usize
}

fn panel_focused(state: &AppState, panel_index: usize) -> bool {
    state.ui.page.normalize_focus(state.ui.focus_index) == panel_index
}

fn risk_panel_tone(state: &AppState) -> PanelTone {
    match state.risk.risk_level {
        crate::protocol::RiskLevel::Ok => PanelTone::Neutral,
        crate::protocol::RiskLevel::Watch => PanelTone::Warning,
        crate::protocol::RiskLevel::Warning | crate::protocol::RiskLevel::Danger => {
            PanelTone::Danger
        }
    }
}

fn risk_style(theme: &Theme, state: &AppState) -> Style {
    match state.risk.risk_level {
        crate::protocol::RiskLevel::Ok => theme.success(),
        crate::protocol::RiskLevel::Watch => theme.warning(),
        crate::protocol::RiskLevel::Warning | crate::protocol::RiskLevel::Danger => theme.danger(),
    }
}

fn panel_tone_for_connection(kind: ConnectionHealthKind) -> PanelTone {
    match kind {
        ConnectionHealthKind::Healthy => PanelTone::Success,
        ConnectionHealthKind::Degraded | ConnectionHealthKind::Stale => PanelTone::Warning,
        ConnectionHealthKind::Reconnecting => PanelTone::Danger,
    }
}

fn status_tone_for_connection(kind: ConnectionHealthKind) -> StatusTone {
    match kind {
        ConnectionHealthKind::Healthy => StatusTone::Success,
        ConnectionHealthKind::Degraded | ConnectionHealthKind::Stale => StatusTone::Warning,
        ConnectionHealthKind::Reconnecting => StatusTone::Danger,
    }
}

fn stage_tone(stage: CommandTimelineStage) -> StatusTone {
    match stage {
        CommandTimelineStage::Pending | CommandTimelineStage::Accepted => StatusTone::Warning,
        CommandTimelineStage::Ack => StatusTone::Success,
        CommandTimelineStage::Failed | CommandTimelineStage::TimedOut => StatusTone::Danger,
    }
}

fn command_panel_tone(state: &AppState) -> PanelTone {
    match state
        .execution
        .command_timeline
        .front()
        .map(|entry| entry.stage)
    {
        Some(CommandTimelineStage::Ack) => PanelTone::Success,
        Some(CommandTimelineStage::Failed | CommandTimelineStage::TimedOut) => PanelTone::Danger,
        Some(CommandTimelineStage::Pending | CommandTimelineStage::Accepted) => PanelTone::Warning,
        None => PanelTone::Neutral,
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Length(height),
        Constraint::Percentage(50),
    ])
    .flex(Flex::Center)
    .split(area);
    Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(width),
        Constraint::Percentage(50),
    ])
    .flex(Flex::Center)
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        events::{AppEvent, EffectResultEvent},
        locale::Locale,
        protocol::{
            CommandLinks, CommandStatus, CommandType, PendingCommand, RiskEvent, RiskLevel,
        },
        state::{AppState, CommandTimelineEntry, Page},
        store::reduce,
        theme::Theme,
    };

    #[test]
    fn dashboard_render_snapshot_120x18() {
        assert_page_snapshot(
            Page::Dashboard,
            120,
            18,
            "dashboard_render_snapshot_120x18",
            |_| {},
        );
    }

    #[test]
    fn dashboard_render_snapshot_100x16() {
        assert_page_snapshot(
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_100x16",
            |_| {},
        );
    }

    #[test]
    fn dashboard_render_snapshot_80x24() {
        assert_page_snapshot(
            Page::Dashboard,
            80,
            24,
            "dashboard_render_snapshot_80x24",
            |_| {},
        );
    }

    #[test]
    fn dashboard_render_snapshot_zh_cn_100x16() {
        assert_page_snapshot_with_locale(
            Locale::ZhCn,
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_zh_cn_100x16",
            |_| {},
        );
    }

    #[test]
    fn dashboard_render_snapshot_zh_cn_80x24() {
        assert_page_snapshot_with_locale(
            Locale::ZhCn,
            Page::Dashboard,
            80,
            24,
            "dashboard_render_snapshot_zh_cn_80x24",
            |_| {},
        );
    }

    #[test]
    fn grid_render_snapshot_120x18() {
        assert_page_snapshot(Page::Grid, 120, 18, "grid_render_snapshot_120x18", |_| {});
    }

    #[test]
    fn grid_render_snapshot_100x16() {
        assert_page_snapshot(Page::Grid, 100, 16, "grid_render_snapshot_100x16", |_| {});
    }

    #[test]
    fn grid_render_snapshot_80x24() {
        assert_page_snapshot(Page::Grid, 80, 24, "grid_render_snapshot_80x24", |_| {});
    }

    #[test]
    fn grid_render_snapshot_zh_cn_100x16() {
        assert_page_snapshot_with_locale(
            Locale::ZhCn,
            Page::Grid,
            100,
            16,
            "grid_render_snapshot_zh_cn_100x16",
            |_| {},
        );
    }

    #[test]
    fn grid_render_snapshot_zh_cn_80x24() {
        assert_page_snapshot_with_locale(
            Locale::ZhCn,
            Page::Grid,
            80,
            24,
            "grid_render_snapshot_zh_cn_80x24",
            |_| {},
        );
    }

    #[test]
    fn market_render_snapshot_100x16() {
        assert_page_snapshot(
            Page::Market,
            100,
            16,
            "market_render_snapshot_100x16",
            |_| {},
        );
    }

    #[test]
    fn market_render_snapshot_reconnecting_100x16() {
        assert_page_snapshot(
            Page::Market,
            100,
            16,
            "market_render_snapshot_reconnecting_100x16",
            apply_degraded_state,
        );
    }

    #[test]
    fn events_render_snapshot_120x18() {
        assert_page_snapshot(
            Page::Events,
            120,
            18,
            "events_render_snapshot_120x18",
            |_| {},
        );
    }

    #[test]
    fn events_render_snapshot_100x16() {
        assert_page_snapshot(
            Page::Events,
            100,
            16,
            "events_render_snapshot_100x16",
            |_| {},
        );
    }

    #[test]
    fn events_render_snapshot_80x24() {
        assert_page_snapshot(Page::Events, 80, 24, "events_render_snapshot_80x24", |_| {});
    }

    #[test]
    fn dashboard_render_snapshot_degraded_100x16() {
        assert_page_snapshot(
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_degraded_100x16",
            apply_degraded_state,
        );
    }

    #[test]
    fn dashboard_render_snapshot_waiting_first_snapshot_100x16() {
        assert_page_snapshot(
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_waiting_first_snapshot_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot();
            },
        );
    }

    #[test]
    fn dashboard_render_snapshot_waiting_first_snapshot_zh_cn_100x16() {
        assert_page_snapshot_after_mutate_with_locale(
            Locale::ZhCn,
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_waiting_first_snapshot_zh_cn_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot_with_locale(Locale::ZhCn);
            },
        );
    }

    #[test]
    fn help_render_snapshot_waiting_first_snapshot_100x16() {
        assert_page_snapshot(
            Page::Help,
            100,
            16,
            "help_render_snapshot_waiting_first_snapshot_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot();
                state.ui.page = Page::Help;
            },
        );
    }

    #[test]
    fn help_render_snapshot_waiting_first_snapshot_zh_cn_100x16() {
        assert_page_snapshot(
            Page::Help,
            100,
            16,
            "help_render_snapshot_waiting_first_snapshot_zh_cn_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot_with_locale(Locale::ZhCn);
                state.ui.page = Page::Help;
            },
        );
    }

    #[test]
    fn market_render_snapshot_snapshot_retrying_100x16() {
        assert_page_snapshot_after_mutate(
            Page::Market,
            100,
            16,
            "market_render_snapshot_snapshot_retrying_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot();
                reduce(
                    state,
                    AppEvent::EffectResult(EffectResultEvent::SnapshotFailed(
                        "upstream timeout".into(),
                    )),
                );
            },
        );
    }

    #[test]
    fn market_render_snapshot_snapshot_retrying_zh_cn_100x16() {
        assert_page_snapshot_after_mutate_with_locale(
            Locale::ZhCn,
            Page::Market,
            100,
            16,
            "market_render_snapshot_snapshot_retrying_zh_cn_100x16",
            |state| {
                *state = AppState::waiting_first_snapshot_with_locale(Locale::ZhCn);
                state.ui.page = Page::Market;
                reduce(
                    state,
                    AppEvent::EffectResult(EffectResultEvent::SnapshotFailed(
                        "upstream timeout".into(),
                    )),
                );
            },
        );
    }

    #[test]
    fn dashboard_renders_service_ws_status_when_transport_is_down() {
        let rendered = render_page_to_string(Page::Dashboard, 100, 16, |state| {
            state.connection.ws_connected = false;
            state.connection.reconnect_attempt = 2;
            state.connection.reconnect_backoff_ms = 2_000;
            state.connection.http_available = true;
            state.connection.market_ws_connected = true;
            state.connection.user_stream_connected = Some(true);
        });

        assert!(rendered.contains("Svc WS"));
        assert!(rendered.contains("Mkt WS"));
    }

    #[test]
    fn dashboard_keeps_prices_and_degraded_reason_visible_at_common_width() {
        let rendered = render_page_to_string(Page::Dashboard, 100, 16, |state| {
            state.connection.ws_connected = true;
            state.connection.market_ws_connected = true;
            state.connection.http_available = true;
            state.connection.user_stream_connected = Some(false);
            state.connection.stale_age_ms = 0;
        });

        assert!(rendered.contains("Last"));
        assert!(rendered.contains("Mark"));
        assert!(rendered.to_ascii_lowercase().contains("user down"));
    }

    #[test]
    fn waiting_first_snapshot_hides_sample_business_data() {
        let rendered = render_page_to_string(Page::Dashboard, 100, 16, |state| {
            *state = AppState::waiting_first_snapshot();
        });

        assert!(rendered.contains("WAITING SNAPSHOT"));
        assert!(!rendered.contains("2361.48"));
        assert!(!rendered.contains("ord_1001"));
        assert!(!rendered.contains("fill_9001"));
    }

    #[test]
    fn snapshot_retrying_hides_sample_business_data() {
        let rendered = render_page_to_string_after_mutate(Page::Events, 80, 24, |state| {
            *state = AppState::waiting_first_snapshot();
            reduce(
                state,
                AppEvent::EffectResult(EffectResultEvent::SnapshotFailed(
                    "upstream timeout".into(),
                )),
            );
        });

        assert!(rendered.contains("SNAPSHOT FAILED"));
        assert!(!rendered.contains("2361.48"));
        assert!(!rendered.contains("ord_1001"));
        assert!(!rendered.contains("fill_9001"));
    }

    #[test]
    fn events_render_snapshot_reconnecting_80x24() {
        assert_page_snapshot(
            Page::Events,
            80,
            24,
            "events_render_snapshot_reconnecting_80x24",
            apply_degraded_state,
        );
    }

    #[test]
    fn dashboard_render_snapshot_execution_links_100x16() {
        assert_page_snapshot(
            Page::Dashboard,
            100,
            16,
            "dashboard_render_snapshot_execution_links_100x16",
            |state| {
                state.execution.command_timeline.clear();
                state
                    .execution
                    .command_timeline
                    .push_front(CommandTimelineEntry {
                        command_id: "cmd_flatten_linked".into(),
                        command: CommandType::FlattenNow,
                        stage: crate::state::CommandTimelineStage::Ack,
                        summary: "Position flattened.".into(),
                        requested_at: "2025-01-01T00:00:03Z".into(),
                        accepted_at: Some("2025-01-01T00:00:04Z".into()),
                        finished_at: Some("2025-01-01T00:00:05Z".into()),
                        links: CommandLinks {
                            client_order_ids: vec!["flatten_reduce_only_01".into()],
                            order_ids: vec!["ord_0999".into()],
                            trade_ids: vec!["fill_9001".into()],
                        },
                        timeout_at_tick: None,
                    });
                state
                    .execution
                    .command_timeline
                    .push_front(CommandTimelineEntry {
                        command_id: "cmd_cancel_linked".into(),
                        command: CommandType::CancelAll,
                        stage: crate::state::CommandTimelineStage::Ack,
                        summary: "All open orders cancelled.".into(),
                        requested_at: "2025-01-01T00:00:01Z".into(),
                        accepted_at: Some("2025-01-01T00:00:02Z".into()),
                        finished_at: Some("2025-01-01T00:00:03Z".into()),
                        links: CommandLinks {
                            client_order_ids: vec!["grid_buy_01".into()],
                            order_ids: vec!["ord_1001".into()],
                            trade_ids: vec![],
                        },
                        timeout_at_tick: None,
                    });
            },
        );
    }

    #[test]
    fn events_render_snapshot_failure_details_100x16() {
        assert_page_snapshot(
            Page::Events,
            100,
            16,
            "events_render_snapshot_failure_details_100x16",
            |state| {
                state.execution.command_timeline.clear();
                state
                    .execution
                    .command_timeline
                    .push_front(CommandTimelineEntry {
                        command_id: "cmd_timeout_01".into(),
                        command: CommandType::FlattenNow,
                        stage: crate::state::CommandTimelineStage::TimedOut,
                        summary: "flatten timed out while waiting for reduce-only fill".into(),
                        requested_at: "2025-01-01T00:00:06Z".into(),
                        accepted_at: Some("2025-01-01T00:00:07Z".into()),
                        finished_at: Some("2025-01-01T00:00:22Z".into()),
                        links: CommandLinks::default(),
                        timeout_at_tick: None,
                    });
                state
                    .execution
                    .command_timeline
                    .push_front(CommandTimelineEntry {
                        command_id: "cmd_failed_01".into(),
                        command: CommandType::CancelAll,
                        stage: crate::state::CommandTimelineStage::Failed,
                        summary: "exchange rejected cancel-all because the order set changed"
                            .into(),
                        requested_at: "2025-01-01T00:00:02Z".into(),
                        accepted_at: Some("2025-01-01T00:00:03Z".into()),
                        finished_at: Some("2025-01-01T00:00:04Z".into()),
                        links: CommandLinks::default(),
                        timeout_at_tick: None,
                    });
            },
        );
    }

    #[test]
    fn events_page_shows_risk_action_hint() {
        let rendered = render_page_to_string(Page::Events, 100, 16, |state| {
            state.risk.alerts.clear();
            state.risk.alerts.push_front(RiskEvent {
                severity: RiskLevel::Danger,
                code: "STOP_LOSS_TRIGGERED".into(),
                message: "Mark price crossed the configured stop line.".into(),
                created_at: "2025-01-01T00:00:12Z".into(),
                acknowledged_at: None,
            });
        });

        assert!(rendered.contains("Reduce exposure"));
    }

    #[test]
    fn dashboard_exchange_orders_unavailable_message() {
        let rendered = normalized_page_string(Page::Dashboard, 100, 16, |state| {
            state.execution.open_orders_source = crate::protocol::OpenOrdersSource::StrategyMirror;
        });

        assert!(rendered.contains("ExchangeOrders"));
        assert!(rendered.contains("Currentmodeexposesstrategy-ordermirrorsonly."));
        assert!(rendered.contains("N/A"));
    }

    #[test]
    fn dashboard_uses_english_copy_by_default() {
        let rendered = normalized_page_string(Page::Dashboard, 100, 16, |_| {});

        assert!(rendered.contains("ExchangeOrders"));
    }

    #[test]
    fn dashboard_can_render_chinese_copy() {
        let rendered = render_page_to_string(Page::Dashboard, 100, 16, |state| {
            state.ui.locale = crate::locale::Locale::ZhCn;
            state.risk.risk_level = crate::protocol::RiskLevel::Watch;
        });
        let normalized = rendered
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();

        assert!(normalized.contains("交易所挂单"));
        assert!(normalized.contains("风险与告警"));
        assert!(normalized.contains("观察"));
        assert!(!normalized.contains("Watch"));
    }

    #[test]
    fn grid_page_shows_strategy_orders_columns() {
        let rendered = normalized_page_string(Page::Grid, 100, 16, |state| {
            state.execution.open_orders_source = crate::protocol::OpenOrdersSource::ExchangeLive;
        });

        assert!(rendered.contains("StrategyOrders"));
        assert!(rendered.contains("Strategy"));
        assert!(rendered.contains("Placement"));
    }

    #[test]
    fn dashboard_uses_exchange_open_orders_when_real_source_is_live() {
        let rendered = normalized_page_string(Page::Dashboard, 100, 16, |state| {
            state.execution.open_orders_source = crate::protocol::OpenOrdersSource::StrategyMirror;
            state.execution.exchange_open_orders_source =
                crate::protocol::OpenOrdersSource::ExchangeLive;
            state.execution.exchange_open_orders = vec![crate::protocol::OpenOrder {
                order_id: "real_ord_01".into(),
                client_order_id: "real_grid_sell_01".into(),
                side: "sell".into(),
                price: 4510.25,
                qty: 0.2,
                filled_qty: 0.0,
                status: "NEW".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: "2025-01-01T00:00:00Z".into(),
            }];
        });

        assert!(rendered.contains("4510.25"));
        assert!(!rendered.contains("Currentmodeexposesstrategy-ordermirrorsonly."));
    }

    #[test]
    fn help_page_explains_strategy_and_exchange_orders() {
        let rendered = normalized_page_string(Page::Help, 100, 16, |_| {});

        assert!(rendered.contains("StrategyOrders"));
        assert!(rendered.contains("ExchangeOrders"));
        assert!(rendered.contains("theclientcannotprovethestrategyorders"));
    }

    #[test]
    fn help_page_mentions_ctrl_c_exit_shortcut() {
        let rendered = normalized_page_string(Page::Help, 100, 16, |_| {});

        assert!(rendered.contains("q/Ctrl-C"));
    }

    fn assert_page_snapshot<F>(page: Page, width: u16, height: u16, name: &str, mutate: F)
    where
        F: FnOnce(&mut AppState),
    {
        let snapshot = render_page_to_string(page, width, height, mutate);
        assert_snapshot!(name, snapshot);
    }

    fn assert_page_snapshot_with_locale<F>(
        locale: Locale,
        page: Page,
        width: u16,
        height: u16,
        name: &str,
        mutate: F,
    ) where
        F: FnOnce(&mut AppState),
    {
        let snapshot = render_page_to_string_with_locale(locale, page, width, height, mutate);
        assert_snapshot!(name, snapshot);
    }

    fn assert_page_snapshot_after_mutate<F>(
        page: Page,
        width: u16,
        height: u16,
        name: &str,
        mutate: F,
    ) where
        F: FnOnce(&mut AppState),
    {
        let snapshot = render_page_to_string_after_mutate(page, width, height, mutate);
        assert_snapshot!(name, snapshot);
    }

    fn assert_page_snapshot_after_mutate_with_locale<F>(
        locale: Locale,
        page: Page,
        width: u16,
        height: u16,
        name: &str,
        mutate: F,
    ) where
        F: FnOnce(&mut AppState),
    {
        let snapshot =
            render_page_to_string_after_mutate_with_locale(locale, page, width, height, mutate);
        assert_snapshot!(name, snapshot);
    }

    fn render_page_to_string<F>(page: Page, width: u16, height: u16, mutate: F) -> String
    where
        F: FnOnce(&mut AppState),
    {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::sample();
        state.ui.page = page;
        state.ui.width = width;
        state.ui.height = height;
        mutate(&mut state);
        let theme = Theme::default();
        terminal.draw(|frame| draw(frame, &state, &theme)).unwrap();
        buffer_to_string(terminal.backend().buffer(), width, height)
    }

    fn render_page_to_string_with_locale<F>(
        locale: Locale,
        page: Page,
        width: u16,
        height: u16,
        mutate: F,
    ) -> String
    where
        F: FnOnce(&mut AppState),
    {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::sample();
        state.ui.page = page;
        state.ui.locale = locale;
        state.ui.width = width;
        state.ui.height = height;
        mutate(&mut state);
        let theme = Theme::default();
        terminal.draw(|frame| draw(frame, &state, &theme)).unwrap();
        buffer_to_string(terminal.backend().buffer(), width, height)
    }

    fn render_page_to_string_after_mutate<F>(
        page: Page,
        width: u16,
        height: u16,
        mutate: F,
    ) -> String
    where
        F: FnOnce(&mut AppState),
    {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::sample();
        state.ui.width = width;
        state.ui.height = height;
        mutate(&mut state);
        state.ui.page = page;
        let theme = Theme::default();
        terminal.draw(|frame| draw(frame, &state, &theme)).unwrap();
        buffer_to_string(terminal.backend().buffer(), width, height)
    }

    fn render_page_to_string_after_mutate_with_locale<F>(
        locale: Locale,
        page: Page,
        width: u16,
        height: u16,
        mutate: F,
    ) -> String
    where
        F: FnOnce(&mut AppState),
    {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::sample();
        state.ui.locale = locale;
        state.ui.width = width;
        state.ui.height = height;
        mutate(&mut state);
        state.ui.page = page;
        let theme = Theme::default();
        terminal.draw(|frame| draw(frame, &state, &theme)).unwrap();
        buffer_to_string(terminal.backend().buffer(), width, height)
    }

    fn normalized_page_string<F>(page: Page, width: u16, height: u16, mutate: F) -> String
    where
        F: FnOnce(&mut AppState),
    {
        render_page_to_string(page, width, height, mutate)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }

    fn apply_degraded_state(state: &mut AppState) {
        state.connection.http_available = false;
        state.connection.market_ws_connected = false;
        state.connection.user_stream_connected = Some(false);
        state.connection.market_reconnect_backoff_ms = 4_000;
        state.connection.ws_connected = false;
        state.connection.reconnect_attempt = 3;
        state.connection.reconnect_backoff_ms = 4_000;
        state.connection.stale_age_ms = 12_000;
        state.risk.risk_level = RiskLevel::Danger;
        state.risk.alerts.push_front(RiskEvent {
            severity: RiskLevel::Danger,
            code: "BREAKER_NEAR".into(),
            message: "Daily loss is close to breaker threshold.".into(),
            created_at: "2025-01-01T00:00:12Z".into(),
            acknowledged_at: None,
        });
        state.execution.pending_commands.push(PendingCommand {
            command_id: "local_cmd_0099".into(),
            command: CommandType::ShutdownAfterFlatten,
            status: CommandStatus::Accepted,
            requested_at: "T+09s".into(),
        });
        state
            .execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: "local_cmd_0099".into(),
                command: CommandType::ShutdownAfterFlatten,
                stage: CommandTimelineStage::Accepted,
                summary: "Service accepted shutdown request; waiting for final ack.".into(),
                requested_at: "T+09s".into(),
                accepted_at: Some("T+10s".into()),
                finished_at: None,
                links: CommandLinks::default(),
                timeout_at_tick: Some(20),
            });
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                output.push_str(cell.symbol());
            }
            output.push('\n');
        }
        output
    }
}
