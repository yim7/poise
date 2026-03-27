use std::time::{Duration, Instant};

use crate::protocol::{GridCommandType, GridDetailView, GridListItemView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Instance,
    Help,
}

#[derive(Debug, Clone)]
pub struct App {
    pub grids: Vec<GridListItemView>,
    pub current_grid: Option<GridDetailView>,
    pub selected_index: usize,
    pub current_view: View,
    pub should_quit: bool,
    status_message: Option<String>,
    initial_load_pending: bool,
    next_http_retry_at: Instant,
    next_ws_retry_at: Instant,
}

impl App {
    pub fn new(mut grids: Vec<GridListItemView>) -> Self {
        grids.sort_by(|left, right| left.id.cmp(&right.id));

        Self {
            grids,
            current_grid: None,
            selected_index: 0,
            current_view: View::Dashboard,
            should_quit: false,
            status_message: None,
            initial_load_pending: false,
            next_http_retry_at: Instant::now(),
            next_ws_retry_at: Instant::now(),
        }
    }

    pub fn selected_grid_id(&self) -> Option<&str> {
        self.grids
            .get(self.selected_index)
            .map(|grid| grid.id.as_str())
    }

    pub fn current_grid_detail(&self) -> Option<&GridDetailView> {
        self.current_grid
            .as_ref()
            .filter(|detail| self.selected_grid_id() == Some(detail.identity.id.as_str()))
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
            .current_grid
            .as_ref()
            .is_some_and(|detail| self.selected_grid_id() != Some(detail.identity.id.as_str()))
        {
            self.current_grid = None;
        }
    }

    pub fn apply_grid_list_item(&mut self, item: GridListItemView) {
        if let Some(existing) = self.grids.iter_mut().find(|grid| grid.id == item.id) {
            *existing = item;
            return;
        }

        let selected_id = self.selected_grid_id().map(ToOwned::to_owned);
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

    pub fn apply_grid_detail(&mut self, detail: GridDetailView) {
        let selected_matches = self.selected_grid_id() == Some(detail.identity.id.as_str());
        let should_refresh_current = selected_matches
            || self
                .current_grid
                .as_ref()
                .is_some_and(|current| current.identity.id == detail.identity.id);
        if should_refresh_current {
            self.current_grid = Some(detail);
        }
    }

    pub fn is_command_enabled(&self, command: GridCommandType) -> bool {
        self.current_grid_detail()
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
        ExecutionStateView, GridDetailView, GridListItemView, GridStreamEvent, GridStreamPayload,
    };

    use super::{App, View};

    fn grid_list_items() -> Vec<GridListItemView> {
        let mut response: crate::protocol::GridListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/grid_list_response.json"))
                .unwrap();
        let mut eth = response.items[0].clone();
        eth.id = "eth-core".into();
        eth.instrument.symbol = "ETHUSDT".into();
        eth.reference_price = Some(2200.0);
        response.items.push(eth);
        response.items
    }

    fn detail_view(id: &str) -> GridDetailView {
        let mut detail: GridDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/grid_detail_view.json")).unwrap();
        detail.identity.id = id.into();
        detail.identity.instrument.symbol = if id == "eth-core" {
            "ETHUSDT".into()
        } else {
            "BTCUSDT".into()
        };
        detail
    }

    #[test]
    fn apply_grid_detail_updates_current_detail_without_snapshot_cache() {
        let mut app = App::new(grid_list_items());
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        app.apply_grid_detail(detail_view("btc-core"));

        assert_eq!(app.current_grid_detail().unwrap().identity.id, "btc-core");
        assert_eq!(
            app.current_grid_detail().unwrap().position.current_exposure,
            3.5
        );
    }

    #[test]
    fn apply_grid_list_item_updates_dashboard_item() {
        let mut app = App::new(grid_list_items());
        let mut updated = app.grids[0].clone();
        updated.reference_price = Some(102.5);
        updated.execution.state = ExecutionStateView::Paused;

        app.apply_grid_list_item(updated);

        assert_eq!(app.grids[0].reference_price, Some(102.5));
        assert_eq!(app.grids[0].execution.state, ExecutionStateView::Paused);
    }

    #[test]
    fn help_view_restores_previous_view() {
        let mut app = App::new(grid_list_items());
        app.current_view = View::Instance;

        app.enter_help();
        assert_eq!(app.current_view, View::Help);

        app.leave_help();
        assert_eq!(app.current_view, View::Dashboard);
    }

    #[test]
    fn show_instance_for_selected_uses_current_list_selection_and_detail() {
        let mut app = App::new(grid_list_items());
        app.current_view = View::Instance;
        app.apply_grid_detail(detail_view("btc-core"));
        app.show_instance_for_selected();

        assert_eq!(app.current_grid_detail().unwrap().identity.id, "btc-core");

        app.select_next();
        app.show_instance_for_selected();

        assert!(app.current_grid_detail().is_none());
    }

    #[test]
    fn apply_grid_detail_event_updates_current_grid() {
        let mut app = App::new(grid_list_items());
        app.current_view = View::Instance;
        app.show_instance_for_selected();
        app.apply_grid_detail(detail_view("btc-core"));

        let event: GridStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_grid_detail_changed.json"
        ))
        .unwrap();
        let GridStreamPayload::GridDetailChanged { detail } = event.payload else {
            panic!("unexpected payload variant");
        };

        app.apply_grid_detail(detail);

        assert_eq!(
            app.current_grid_detail().unwrap().status.reference_price,
            Some(101.5)
        );
    }
}
