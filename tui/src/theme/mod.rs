use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTone {
    Neutral,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Muted,
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg_base: Color,
    pub bg_panel: Color,
    pub fg_primary: Color,
    pub fg_muted: Color,
    pub accent_primary: Color,
    pub accent_secondary: Color,
    pub profit: Color,
    pub warning: Color,
    pub danger: Color,
    pub border_idle: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg_base: Color::Rgb(26, 27, 38),
            bg_panel: Color::Rgb(36, 40, 59),
            fg_primary: Color::Rgb(192, 202, 245),
            fg_muted: Color::Rgb(86, 95, 137),
            accent_primary: Color::Rgb(42, 195, 222),
            accent_secondary: Color::Rgb(122, 162, 247),
            profit: Color::Rgb(158, 206, 106),
            warning: Color::Rgb(224, 175, 104),
            danger: Color::Rgb(247, 118, 142),
            border_idle: Color::Rgb(65, 72, 104),
        }
    }
}

impl Theme {
    pub fn panel(&self) -> Style {
        Style::default().fg(self.fg_primary).bg(self.bg_panel)
    }

    pub fn panel_border(&self, tone: PanelTone, focused: bool) -> Style {
        let color = if focused {
            self.accent_primary
        } else {
            match tone {
                PanelTone::Neutral => self.border_idle,
                PanelTone::Success => self.profit,
                PanelTone::Warning => self.warning,
                PanelTone::Danger => self.danger,
            }
        };
        Style::default().fg(color)
    }

    pub fn panel_title(&self, tone: PanelTone, focused: bool) -> Style {
        let base = match tone {
            PanelTone::Neutral => self.fg_primary,
            PanelTone::Success => self.profit,
            PanelTone::Warning => self.warning,
            PanelTone::Danger => self.danger,
        };
        Style::default()
            .fg(if focused { self.accent_primary } else { base })
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted).bg(self.bg_panel)
    }

    pub fn info(&self) -> Style {
        Style::default()
            .fg(self.accent_secondary)
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn emphasis(&self) -> Style {
        Style::default()
            .fg(self.accent_primary)
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn success(&self) -> Style {
        Style::default()
            .fg(self.profit)
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn warning(&self) -> Style {
        Style::default()
            .fg(self.warning)
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn danger(&self) -> Style {
        Style::default()
            .fg(self.danger)
            .bg(self.bg_panel)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status(&self, tone: StatusTone) -> Style {
        match tone {
            StatusTone::Muted => self.muted(),
            StatusTone::Info => self.info(),
            StatusTone::Success => self.success(),
            StatusTone::Warning => self.warning(),
            StatusTone::Danger => self.danger(),
        }
    }

    pub fn badge(&self, tone: StatusTone) -> Style {
        let background = match tone {
            StatusTone::Muted => self.border_idle,
            StatusTone::Info => self.accent_secondary,
            StatusTone::Success => self.profit,
            StatusTone::Warning => self.warning,
            StatusTone::Danger => self.danger,
        };
        Style::default()
            .fg(self.bg_base)
            .bg(background)
            .add_modifier(Modifier::BOLD)
    }
}
