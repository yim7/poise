use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::app::App;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(0),
        ])
        .split(area);

    let Some(snapshot) = app
        .current_instance
        .as_ref()
        .or_else(|| app.selected_snapshot())
    else {
        let empty = Paragraph::new("No instance snapshot loaded")
            .block(Block::default().title("Instance").borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };

    let summary_lines = vec![
        Line::from(format!("id: {}", snapshot.id)),
        Line::from(format!("symbol: {}", snapshot.symbol)),
        Line::from(format!("status: {}", snapshot.status)),
        Line::from(format!("actual exposure: {:.4}", snapshot.current_exposure)),
        Line::from(format!(
            "target exposure: {}",
            snapshot
                .target_exposure()
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "-".to_string())
        )),
        Line::from(format!(
            "pending order: {}",
            snapshot
                .pending_order
                .as_ref()
                .map(|order| format!(
                    "{} {:.4} @ {:.4} ({})",
                    order.side, order.quantity, order.price, order.client_order_id
                ))
                .unwrap_or_else(|| "none".to_string())
        )),
        Line::from(format!("band state: {}", snapshot.band_state())),
    ];
    let summary = Paragraph::new(summary_lines)
        .block(Block::default().title("Overview").borders(Borders::ALL));
    frame.render_widget(summary, sections[0]);

    let config_lines = vec![
        Line::from(format!("lower: {:.4}", snapshot.config.lower_price)),
        Line::from(format!("upper: {:.4}", snapshot.config.upper_price)),
        Line::from(format!("long cap: {:.4}", snapshot.config.long_capacity)),
        Line::from(format!("short cap: {:.4}", snapshot.config.short_capacity)),
        Line::from(format!(
            "capacity notional: {:.4}",
            snapshot.config.capacity_notional
        )),
        Line::from(format!("shape: {}", snapshot.config.shape_family)),
        Line::from(format!(
            "out of band policy: {}",
            snapshot.config.out_of_band_policy
        )),
    ];
    let config =
        Paragraph::new(config_lines).block(Block::default().title("Config").borders(Borders::ALL));
    frame.render_widget(config, sections[1]);

    let items: Vec<ListItem<'_>> = app
        .recent_events_for_current()
        .into_iter()
        .map(|event| ListItem::new(event.event.to_string()))
        .collect();
    let items = if items.is_empty() {
        vec![ListItem::new("No events yet")]
    } else {
        items
    };
    let events = List::new(items).block(
        Block::default()
            .title("Recent Events")
            .borders(Borders::ALL),
    );
    frame.render_widget(events, sections[2]);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, View};
    use crate::protocol::{
        DomainEvent, GridConfig, InstanceSnapshot, InstanceStatus, InstanceSummary,
        OutOfBandPolicy, PendingOrder, ShapeFamily, Side, WsEvent,
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

    #[test]
    fn renders_instance_details_and_events() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(vec![InstanceSummary {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            last_price: Some(100.0),
        }]);
        app.current_view = View::Instance;
        app.apply_snapshot(InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            current_exposure: 1.0,
            target_exposure: Some(4.0),
            last_price: Some(100.0),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("12345".into()),
                client_order_id: "btc-grid-1".into(),
                side: Side::Buy,
                price: 90.0,
                quantity: 0.5,
                status: "NEW".into(),
            }),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Concave,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        });
        app.show_instance_for_selected();
        app.record_event(WsEvent {
            instance_id: "BTCUSDT".into(),
            event: DomainEvent::BandReentered { price: 99.0 },
        });

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Overview"));
        assert!(text.contains("actual exposure"));
        assert!(text.contains("target exposure"));
        assert!(text.contains("pending order"));
        assert!(text.contains("band state"));
        assert!(text.contains("capacity notional"));
        assert!(text.contains("band reentered"));
    }
}
