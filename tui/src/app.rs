use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::protocol::{InstanceSnapshot, InstanceSummary, WsEvent};

const MAX_RECENT_EVENTS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Instance,
    Help,
}

#[derive(Debug, Clone)]
pub struct App {
    pub instances: Vec<InstanceSummary>,
    pub current_instance: Option<InstanceSnapshot>,
    pub selected_index: usize,
    pub current_view: View,
    pub should_quit: bool,
    snapshot_cache: HashMap<String, InstanceSnapshot>,
    recent_events: HashMap<String, VecDeque<WsEvent>>,
    status_message: Option<String>,
    initial_load_pending: bool,
    next_http_retry_at: Instant,
    next_ws_retry_at: Instant,
}

impl App {
    pub fn new(mut instances: Vec<InstanceSummary>) -> Self {
        instances.sort_by(|left, right| left.id.cmp(&right.id));

        Self {
            instances,
            current_instance: None,
            selected_index: 0,
            current_view: View::Dashboard,
            should_quit: false,
            snapshot_cache: HashMap::new(),
            recent_events: HashMap::new(),
            status_message: None,
            initial_load_pending: false,
            next_http_retry_at: Instant::now(),
            next_ws_retry_at: Instant::now(),
        }
    }

    pub fn selected_instance_id(&self) -> Option<&str> {
        self.instances
            .get(self.selected_index)
            .map(|instance| instance.id.as_str())
    }

    pub fn selected_snapshot(&self) -> Option<&InstanceSnapshot> {
        self.selected_instance_id()
            .and_then(|id| self.snapshot_cache.get(id))
    }

    pub fn cached_snapshot(&self, id: &str) -> Option<&InstanceSnapshot> {
        self.snapshot_cache.get(id)
    }

    pub fn recent_events_for_current(&self) -> Vec<WsEvent> {
        let Some(instance_id) = self
            .current_instance
            .as_ref()
            .map(|snapshot| snapshot.id.as_str())
            .or_else(|| self.selected_instance_id())
        else {
            return vec![];
        };

        self.recent_events
            .get(instance_id)
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn select_next(&mut self) {
        if self.instances.is_empty() {
            return;
        }

        self.selected_index = (self.selected_index + 1) % self.instances.len();
    }

    pub fn select_previous(&mut self) {
        if self.instances.is_empty() {
            return;
        }

        self.selected_index = if self.selected_index == 0 {
            self.instances.len() - 1
        } else {
            self.selected_index - 1
        };
    }

    pub fn enter_help(&mut self) {
        self.current_view = View::Help;
    }

    pub fn leave_help(&mut self) {
        self.show_dashboard();
    }

    pub fn show_dashboard(&mut self) {
        self.current_view = View::Dashboard;
        self.current_instance = None;
    }

    pub fn show_instance_for_selected(&mut self) {
        self.current_view = View::Instance;
        self.current_instance = self.selected_snapshot().cloned();
    }

    pub fn has_current_instance(&self) -> bool {
        self.current_instance.is_some()
    }

    pub fn apply_snapshot(&mut self, snapshot: InstanceSnapshot) {
        if let Some(summary) = self
            .instances
            .iter_mut()
            .find(|item| item.id == snapshot.id)
        {
            summary.status = snapshot.status.clone();
            summary.last_price = snapshot.last_price;
        }

        let selected_matches = self.selected_instance_id() == Some(snapshot.id.as_str());
        let should_refresh_current = (selected_matches && self.current_view == View::Instance)
            || self
                .current_instance
                .as_ref()
                .is_some_and(|current| current.id == snapshot.id);
        if should_refresh_current {
            self.current_instance = Some(snapshot.clone());
        }

        self.snapshot_cache.insert(snapshot.id.clone(), snapshot);
    }

    pub fn record_event(&mut self, event: WsEvent) {
        let events = self
            .recent_events
            .entry(event.instance_id.clone())
            .or_default();
        events.push_front(event);
        while events.len() > MAX_RECENT_EVENTS {
            events.pop_back();
        }
    }

    pub fn set_status_message(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    pub fn should_retry_websocket(&self) -> bool {
        Instant::now() >= self.next_ws_retry_at
    }

    pub fn schedule_websocket_retry(&mut self, delay: Duration) {
        self.next_ws_retry_at = Instant::now() + delay;
    }

    pub fn mark_websocket_connected(&mut self) {
        self.next_ws_retry_at = Instant::now();
    }

    pub fn schedule_initial_load_retry(&mut self, delay: Duration) {
        self.initial_load_pending = true;
        self.next_http_retry_at = Instant::now() + delay;
    }

    pub fn should_retry_initial_load(&self) -> bool {
        self.initial_load_pending && Instant::now() >= self.next_http_retry_at
    }

    pub fn mark_initial_load_complete(&mut self) {
        self.initial_load_pending = false;
        self.next_http_retry_at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{
        DomainEvent, GridConfig, InstanceSnapshot, InstanceStatus, InstanceSummary,
        OutOfBandPolicy, ShapeFamily, WsEvent,
    };

    use super::{App, View};

    fn summary(id: &str) -> InstanceSummary {
        InstanceSummary {
            id: id.into(),
            symbol: id.into(),
            status: InstanceStatus::WaitingMarketData,
            last_price: None,
        }
    }

    fn snapshot(id: &str, exposure: f64) -> InstanceSnapshot {
        InstanceSnapshot {
            id: id.into(),
            symbol: id.into(),
            status: InstanceStatus::Active,
            current_exposure: exposure,
            last_price: Some(100.0),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        }
    }

    #[test]
    fn applies_snapshot_to_summary_and_selected_instance() {
        let mut app = App::new(vec![summary("BTCUSDT")]);
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        app.apply_snapshot(snapshot("BTCUSDT", 3.5));

        assert_eq!(app.instances[0].status, InstanceStatus::Active);
        assert_eq!(app.instances[0].last_price, Some(100.0));
        assert_eq!(app.current_instance.unwrap().current_exposure, 3.5);
    }

    #[test]
    fn records_recent_events_with_limit() {
        let mut app = App::new(vec![summary("BTCUSDT")]);

        for index in 0..25 {
            app.record_event(WsEvent {
                instance_id: "BTCUSDT".into(),
                event: DomainEvent::BandReentered {
                    price: index as f64,
                },
            });
        }

        let events = app.recent_events_for_current();
        assert_eq!(events.len(), 20);
        assert_eq!(events[0].event, DomainEvent::BandReentered { price: 24.0 });
    }

    #[test]
    fn help_view_restores_previous_view() {
        let mut app = App::new(vec![summary("BTCUSDT")]);
        app.current_view = View::Instance;

        app.enter_help();
        assert_eq!(app.current_view, View::Help);

        app.leave_help();
        assert_eq!(app.current_view, View::Dashboard);
    }

    #[test]
    fn show_instance_for_selected_clears_stale_snapshot_when_missing() {
        let mut app = App::new(vec![summary("BTCUSDT"), summary("ETHUSDT")]);
        app.current_view = View::Instance;
        app.apply_snapshot(snapshot("BTCUSDT", 3.5));
        app.show_instance_for_selected();

        app.select_next();
        app.show_instance_for_selected();

        assert!(app.current_instance.is_none());
    }
}
