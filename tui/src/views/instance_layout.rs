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
    pub status: Rect,
    pub overview: Rect,
    pub strategy: Rect,
    pub execution: Rect,
    pub statistics: Option<Rect>,
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

pub fn resolve_detail_layout(area: Rect) -> DetailSections {
    let mode = if area.height >= 30 {
        DetailLayoutMode::Standard
    } else if area.height >= 20 {
        DetailLayoutMode::Compact
    } else {
        DetailLayoutMode::Minimal
    };

    match mode {
        DetailLayoutMode::Standard => {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(6),
                    Constraint::Length(9),
                    Constraint::Length(5),
                    Constraint::Min(0),
                ])
                .split(area);

            DetailSections {
                mode,
                status: sections[0],
                overview: sections[1],
                strategy: sections[2],
                execution: sections[3],
                statistics: Some(sections[4]),
                trace: Some(sections[5]),
            }
        }
        DetailLayoutMode::Compact => {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(4),
                    Constraint::Length(5),
                    Constraint::Length(6),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(area);

            DetailSections {
                mode,
                status: sections[0],
                overview: sections[1],
                strategy: sections[2],
                execution: sections[3],
                statistics: Some(sections[4]),
                trace: Some(sections[5]),
            }
        }
        DetailLayoutMode::Minimal => {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(4),
                    Constraint::Min(0),
                ])
                .split(area);

            DetailSections {
                mode,
                status: sections[0],
                overview: sections[1],
                strategy: sections[2],
                execution: sections[3],
                statistics: None,
                trace: None,
            }
        }
    }
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
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 36));

        assert_eq!(layout.mode, DetailLayoutMode::Standard);
    }

    #[test]
    fn selects_compact_layout_for_medium_body() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 24));

        assert_eq!(layout.mode, DetailLayoutMode::Compact);
    }

    #[test]
    fn selects_minimal_layout_for_short_body_and_hides_secondary_sections() {
        let layout = resolve_detail_layout(Rect::new(0, 0, 100, 16));

        assert_eq!(layout.mode, DetailLayoutMode::Minimal);
        assert!(layout.statistics.is_none());
        assert!(layout.trace.is_none());
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
