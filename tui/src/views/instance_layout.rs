use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLayoutMode {
    Standard,
    Compact,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailSections {
    pub mode: DetailLayoutMode,
    pub track: Rect,
    pub pnl: Option<Rect>,
    pub market: Rect,
    pub strategy: Option<Rect>,
    pub execution: Rect,
    pub trace: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceLayout {
    pub activity: TraceSectionLayout,
    pub diagnostics: Option<TraceSectionLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceSectionLayout {
    pub area: Rect,
    pub max_entries: usize,
}

const MIN_TRACE_SECTION_HEIGHT: u16 = 4;
const STANDARD_EXECUTION_MIN_HEIGHT: u16 = 4;
const STANDARD_EXECUTION_MAX_HEIGHT: u16 = 14;

pub fn resolve_detail_layout(area: Rect, execution_body_lines: usize) -> DetailSections {
    let mode = if area.height >= 30 {
        DetailLayoutMode::Standard
    } else if area.height >= 23 {
        DetailLayoutMode::Compact
    } else {
        DetailLayoutMode::Minimal
    };

    match mode {
        DetailLayoutMode::Standard => {
            let execution_height = standard_execution_height(area.height, execution_body_lines);
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(3),
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(execution_height),
                    Constraint::Min(0),
                ])
                .split(area);

            DetailSections {
                mode,
                track: sections[0],
                pnl: Some(sections[1]),
                market: sections[2],
                strategy: Some(sections[3]),
                execution: sections[4],
                trace: trace_panel_area(sections[5]),
            }
        }
        DetailLayoutMode::Compact => {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(3),
                    Constraint::Length(5),
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(area);

            DetailSections {
                mode,
                track: sections[0],
                pnl: Some(sections[1]),
                market: sections[2],
                strategy: Some(sections[3]),
                execution: sections[4],
                trace: trace_panel_area(sections[5]),
            }
        }
        DetailLayoutMode::Minimal => {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(3),
                ])
                .split(area);

            DetailSections {
                mode,
                track: sections[0],
                pnl: Some(sections[1]),
                market: sections[2],
                strategy: Some(sections[3]),
                execution: sections[4],
                trace: None,
            }
        }
    }
}

fn standard_execution_height(area_height: u16, execution_body_lines: usize) -> u16 {
    let requested = (execution_body_lines as u16)
        .saturating_add(2)
        .clamp(STANDARD_EXECUTION_MIN_HEIGHT, STANDARD_EXECUTION_MAX_HEIGHT);
    let fixed_height = 5 + 3 + 5 + 5;
    let available_after_fixed = area_height.saturating_sub(fixed_height);
    let max_without_hiding_trace = available_after_fixed.saturating_sub(MIN_TRACE_SECTION_HEIGHT);
    requested
        .min(max_without_hiding_trace.max(STANDARD_EXECUTION_MIN_HEIGHT))
        .max(STANDARD_EXECUTION_MIN_HEIGHT)
}

fn trace_panel_area(area: Rect) -> Option<Rect> {
    (area.height >= MIN_TRACE_SECTION_HEIGHT).then_some(area)
}

pub fn resolve_trace_layout(area: Rect, show_diagnostics: bool) -> TraceLayout {
    if !show_diagnostics || area.height < 4 {
        return TraceLayout {
            activity: trace_section_layout(area),
            diagnostics: None,
        };
    }

    let diagnostics_height = (area.height / 3).max(2).min(area.height.saturating_sub(2));
    let activity_height = area.height.saturating_sub(diagnostics_height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(activity_height),
            Constraint::Length(diagnostics_height),
        ])
        .split(area);

    TraceLayout {
        activity: trace_section_layout(sections[0]),
        diagnostics: Some(trace_section_layout(sections[1])),
    }
}

fn trace_section_layout(area: Rect) -> TraceSectionLayout {
    TraceSectionLayout {
        area,
        max_entries: area.height.saturating_sub(1) as usize,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::{DetailLayoutMode, resolve_detail_layout, resolve_trace_layout};

    #[test]
    fn selects_standard_layout_for_tall_body() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 36), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Standard);
        assert_eq!(layout.track.height, 5);
        assert_eq!(layout.market.height, 5);
        assert!(layout.pnl.is_some());
    }

    #[test]
    fn selects_compact_layout_for_medium_body() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 24), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Compact);
    }

    #[test]
    fn keeps_compact_layout_off_until_fixed_sections_fit() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 22), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Minimal);
    }

    #[test]
    fn enters_compact_layout_once_boundary_height_is_available() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 23), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Compact);
        assert!(layout.trace.is_none());
    }

    #[test]
    fn keeps_compact_trace_hidden_until_panel_body_fits() {
        let hidden = resolve_detail_layout(Rect::new(0, 0, 100, 23), 6);
        let visible = resolve_detail_layout(Rect::new(0, 0, 100, 25), 6);

        assert_eq!(hidden.mode, DetailLayoutMode::Compact);
        assert!(hidden.trace.is_none());
        assert_eq!(visible.mode, DetailLayoutMode::Compact);
        assert_eq!(visible.trace.map(|area| area.height), Some(5));
    }

    #[test]
    fn selects_minimal_layout_for_short_body_and_hides_secondary_sections() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 16), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Minimal);
        assert!(layout.pnl.is_some());
        assert!(layout.strategy.is_some());
        assert!(layout.trace.is_none());
    }

    #[test]
    fn preserves_execution_body_at_minimal_height_boundary() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 15), 6);

        assert_eq!(layout.mode, DetailLayoutMode::Minimal);
        assert!(layout.execution.height >= 3);
    }

    #[test]
    fn sizes_standard_execution_panel_from_runtime_line_count() {
        let compact_execution = resolve_detail_layout(Rect::new(0, 0, 100, 36), 2);
        let expanded_execution = resolve_detail_layout(Rect::new(0, 0, 100, 36), 10);

        assert!(expanded_execution.execution.height > compact_execution.execution.height);
        assert!(expanded_execution.trace.is_some());
        assert!(expanded_execution.execution.height <= 12);
    }

    #[test]
    fn splits_trace_area_when_diagnostics_is_enabled() {
        let trace = resolve_trace_layout(Rect::new(0, 0, 100, 8), true);

        assert_eq!(trace.activity.area.height, 6);
        assert_eq!(trace.activity.max_entries, 5);
        assert_eq!(
            trace
                .diagnostics
                .map(|layout| (layout.area.height, layout.max_entries)),
            Some((2, 1))
        );
    }

    #[test]
    fn keeps_trace_as_single_section_without_diagnostics() {
        let trace = resolve_trace_layout(Rect::new(0, 0, 100, 8), false);

        assert_eq!(trace.activity.area.height, 8);
        assert_eq!(trace.activity.max_entries, 7);
        assert!(trace.diagnostics.is_none());
    }

    #[test]
    fn keeps_trace_as_single_section_when_height_is_too_small_to_split() {
        let trace = resolve_trace_layout(Rect::new(0, 0, 100, 3), true);

        assert_eq!(trace.activity.area.height, 3);
        assert_eq!(trace.activity.max_entries, 2);
        assert!(trace.diagnostics.is_none());
    }

    #[test]
    fn splits_trace_area_at_minimum_supported_height() {
        let trace = resolve_trace_layout(Rect::new(0, 0, 100, 4), true);

        assert_eq!(trace.activity.area.height, 2);
        assert_eq!(trace.activity.max_entries, 1);
        assert_eq!(
            trace
                .diagnostics
                .map(|layout| (layout.area.height, layout.max_entries)),
            Some((2, 1))
        );
    }

    #[test]
    fn splits_trace_area_preserving_extra_row_for_activity() {
        let trace = resolve_trace_layout(Rect::new(0, 0, 100, 5), true);

        assert_eq!(trace.activity.area.height, 3);
        assert_eq!(trace.activity.max_entries, 2);
        assert_eq!(
            trace
                .diagnostics
                .map(|layout| (layout.area.height, layout.max_entries)),
            Some((2, 1))
        );
    }
}
