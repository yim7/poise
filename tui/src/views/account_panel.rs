use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::protocol::{AccountSummaryView, RiskSignalView};
use crate::theme::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, summary: Option<&AccountSummaryView>) {
    let panel = Paragraph::new(lines(summary))
        .block(Block::default().title("Account").borders(Borders::ALL));
    frame.render_widget(panel, area);
}

fn lines(summary: Option<&AccountSummaryView>) -> Vec<Line<'static>> {
    let Some(summary) = summary else {
        return vec![
            Line::from("unavailable"),
            Line::from("waiting for account summary"),
        ];
    };

    vec![
        Line::from(format!(
            "equity {} | available {} | unrealized pnl {} | day change {}",
            format_optional_amount(summary.equity),
            format_optional_amount(summary.available),
            format_optional_amount(summary.unrealized_pnl),
            format_optional_percent(summary.day_change_pct),
        )),
        Line::from(vec![
            Span::styled(
                signal_label(summary.risk_signal).to_string(),
                signal_style(summary.risk_signal),
            ),
            Span::raw(format!(
                " | reason {} | day base {} | updated {}",
                summary.reason.as_deref().unwrap_or("-"),
                summary.day_base_at.as_deref().unwrap_or("-"),
                summary.updated_at.as_deref().unwrap_or("-"),
            )),
        ]),
    ]
}

fn format_optional_amount(value: Option<f64>) -> String {
    value.map(format_amount).unwrap_or_else(|| "-".to_string())
}

fn format_optional_percent(value: Option<f64>) -> String {
    value.map(|value| format!("{value:+.1}%"))
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

fn signal_label(signal: RiskSignalView) -> &'static str {
    match signal {
        RiskSignalView::Normal => "normal",
        RiskSignalView::Attention => "attention",
        RiskSignalView::Critical => "critical",
    }
}

fn signal_style(signal: RiskSignalView) -> ratatui::style::Style {
    match signal {
        RiskSignalView::Normal => Theme::status_neutral(),
        RiskSignalView::Attention => Theme::status_attention(),
        RiskSignalView::Critical => Theme::status_alert(),
    }
}
