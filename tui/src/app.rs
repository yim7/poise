use std::time::{Duration, Instant};

use crate::protocol::{GridCommandType, TrackDetailView, TrackDiagnosticsView, TrackListItemView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Instance,
    Help,
}

#[derive(Debug, Clone)]
pub struct App {
    pub grids: Vec<TrackListItemView>,
    pub current_track: Option<TrackDetailView>,
    pub selected_index: usize,
    pub current_view: View,
    pub should_quit: bool,
    debug_diagnostics_enabled: bool,
    current_track_diagnostics: Option<TrackDiagnosticsView>,
    current_track_diagnostics_track_id: Option<String>,
    status_message: Option<String>,
    initial_load_pending: bool,
    next_http_retry_at: Instant,
    next_ws_retry_at: Instant,
}

impl App {
    pub fn new(mut grids: Vec<TrackListItemView>) -> Self {
        grids.sort_by(|left, right| left.id.cmp(&right.id));

        Self {
            grids,
            current_track: None,
            selected_index: 0,
            current_view: View::Dashboard,
            should_quit: false,
            debug_diagnostics_enabled: false,
            current_track_diagnostics: None,
            current_track_diagnostics_track_id: None,
            status_message: None,
            initial_load_pending: false,
            next_http_retry_at: Instant::now(),
            next_ws_retry_at: Instant::now(),
        }
    }

    pub fn selected_track_id(&self) -> Option<&str> {
        self.grids
            .get(self.selected_index)
            .map(|grid| grid.id.as_str())
    }

    pub fn selected_grid(&self) -> Option<&TrackListItemView> {
        self.grids.get(self.selected_index)
    }

    pub fn current_track_detail(&self) -> Option<&TrackDetailView> {
        self.current_track
            .as_ref()
            .filter(|detail| self.selected_track_id() == Some(detail.identity.id.as_str()))
    }

    pub fn current_track_diagnostics(&self) -> Option<&TrackDiagnosticsView> {
        self.current_track_diagnostics
            .as_ref()
            .filter(|_| {
                self.current_track_diagnostics_track_id.as_deref() == self.selected_track_id()
            })
    }

    pub fn select_next(&mut self) {
        if self.grids.is_empty() {
            return;
        }

        self.selected_index = (self.selected_index + 1) % self.grids.len();
    }

    pub fn select_previous(&mut self) {
        if self.grids.is_empty() {
            return;
        }

        self.selected_index = if self.selected_index == 0 {
            self.grids.len() - 1
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
    }

    pub fn show_instance_for_selected(&mut self) {
        self.current_view = View::Instance;
        if self
            .current_track
            .as_ref()
            .is_some_and(|detail| self.selected_track_id() != Some(detail.identity.id.as_str()))
        {
            self.current_track = None;
        }
    }

    pub fn apply_track_list_item(&mut self, item: TrackListItemView) {
        if let Some(existing) = self.grids.iter_mut().find(|grid| grid.id == item.id) {
            *existing = item;
            return;
        }

        let selected_id = self.selected_track_id().map(ToOwned::to_owned);
        self.grids.push(item);
        self.grids.sort_by(|left, right| left.id.cmp(&right.id));
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.grids.iter().position(|grid| grid.id == selected_id) {
                self.selected_index = index;
            }
        } else if !self.grids.is_empty() {
            self.selected_index = self.selected_index.min(self.grids.len() - 1);
        }
    }

    pub fn apply_track_detail(&mut self, detail: TrackDetailView) {
        let selected_matches = self.selected_track_id() == Some(detail.identity.id.as_str());
        let should_refresh_current = selected_matches
            || self
                .current_track
                .as_ref()
                .is_some_and(|current| current.identity.id == detail.identity.id);
        if should_refresh_current {
            self.current_track = Some(detail);
        }
    }

    pub fn debug_diagnostics_enabled(&self) -> bool {
        self.debug_diagnostics_enabled
    }

    pub fn set_debug_diagnostics_enabled(&mut self, enabled: bool) {
        self.debug_diagnostics_enabled = enabled;
        if !enabled {
            self.clear_track_diagnostics();
        }
    }

    pub fn toggle_debug_diagnostics(&mut self) -> bool {
        let enabled = !self.debug_diagnostics_enabled;
        self.set_debug_diagnostics_enabled(enabled);
        enabled
    }

    pub fn apply_track_diagnostics(&mut self, diagnostics: TrackDiagnosticsView) {
        let Some(track_id) = self.selected_track_id().map(ToOwned::to_owned) else {
            return;
        };

        self.current_track_diagnostics = Some(diagnostics);
        self.current_track_diagnostics_track_id = Some(track_id);
    }

    pub fn clear_track_diagnostics(&mut self) {
        self.current_track_diagnostics = None;
        self.current_track_diagnostics_track_id = None;
    }

    pub fn is_command_enabled(&self, command: GridCommandType) -> bool {
        self.current_track_detail()
            .and_then(|detail| {
                detail
                    .available_commands
                    .iter()
                    .find(|candidate| candidate.command == command)
            })
            .is_some_and(|candidate| candidate.enabled)
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
        ExecutionStateView, TrackDetailView, TrackDiagnosticsView, TrackListItemView,
        TrackStreamEvent, TrackStreamPayload,
    };

    use super::{App, View};

    fn track_list_items() -> Vec<TrackListItemView> {
        let mut response: crate::protocol::TrackListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json"))
                .unwrap();
        let mut eth = response.items[0].clone();
        eth.id = "eth-core".into();
        eth.instrument.symbol = "ETHUSDT".into();
        eth.reference_price = Some(2200.0);
        response.items.push(eth);
        response.items
    }

    fn detail_view(id: &str) -> TrackDetailView {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap();
        detail.identity.id = id.into();
        detail.identity.instrument.symbol = if id == "eth-core" {
            "ETHUSDT".into()
        } else {
            "BTCUSDT".into()
        };
        detail
    }

    fn diagnostics_view() -> TrackDiagnosticsView {
        serde_json::from_str(include_str!("../tests/fixtures/track_diagnostics_view.json"))
            .unwrap()
    }

    #[test]
    fn apply_track_detail_updates_current_detail_without_snapshot_cache() {
        let mut app = App::new(track_list_items());
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        app.apply_track_detail(detail_view("btc-core"));

        assert_eq!(app.current_track_detail().unwrap().identity.id, "btc-core");
        assert_eq!(
            app.current_track_detail()
                .unwrap()
                .position
                .current_exposure,
            3.5
        );
    }

    #[test]
    fn apply_track_list_item_updates_dashboard_item() {
        let mut app = App::new(track_list_items());
        let mut updated = app.grids[0].clone();
        updated.reference_price = Some(102.5);
        updated.execution.state = ExecutionStateView::Paused;

        app.apply_track_list_item(updated);

        assert_eq!(app.grids[0].reference_price, Some(102.5));
        assert_eq!(app.grids[0].execution.state, ExecutionStateView::Paused);
    }

    #[test]
    fn help_view_restores_previous_view() {
        let mut app = App::new(track_list_items());
        app.current_view = View::Instance;

        app.enter_help();
        assert_eq!(app.current_view, View::Help);

        app.leave_help();
        assert_eq!(app.current_view, View::Dashboard);
    }

    #[test]
    fn show_instance_for_selected_uses_current_list_selection_and_detail() {
        let mut app = App::new(track_list_items());
        app.current_view = View::Instance;
        app.apply_track_detail(detail_view("btc-core"));
        app.show_instance_for_selected();

        assert_eq!(app.current_track_detail().unwrap().identity.id, "btc-core");

        app.select_next();
        app.show_instance_for_selected();

        assert!(app.current_track_detail().is_none());
    }

    #[test]
    fn apply_track_detail_event_updates_current_track() {
        let mut app = App::new(track_list_items());
        app.current_view = View::Instance;
        app.show_instance_for_selected();
        app.apply_track_detail(detail_view("btc-core"));

        let event: TrackStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_detail_changed.json"
        ))
        .unwrap();
        let TrackStreamPayload::TrackDetailChanged { detail } = event.payload else {
            panic!("unexpected payload variant");
        };

        app.apply_track_detail(*detail);

        assert_eq!(
            app.current_track_detail().unwrap().status.reference_price,
            Some(101.5)
        );
    }

    #[test]
    fn track_diagnostics_follow_debug_visibility_and_selected_track() {
        let mut app = App::new(track_list_items());
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        assert!(!app.debug_diagnostics_enabled());
        assert!(app.current_track_diagnostics().is_none());

        assert!(app.toggle_debug_diagnostics());
        app.apply_track_diagnostics(diagnostics_view());
        assert_eq!(app.current_track_diagnostics().unwrap().items.len(), 1);

        app.select_next();
        assert!(app.current_track_diagnostics().is_none());

        app.set_debug_diagnostics_enabled(false);
        assert!(app.current_track_diagnostics().is_none());
    }
}
