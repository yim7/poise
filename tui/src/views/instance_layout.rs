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
    pub show_statistics: bool,
    pub show_trace: bool,
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
                show_statistics: true,
                show_trace: true,
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
                show_statistics: true,
                show_trace: true,
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
                show_statistics: false,
                show_trace: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::{DetailLayoutMode, resolve_detail_layout};

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
        assert!(!layout.show_statistics);
        assert!(!layout.show_trace);
    }
}
