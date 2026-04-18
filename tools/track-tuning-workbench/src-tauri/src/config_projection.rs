use crate::config_document::TrackDraft;

pub fn export_current_track(draft: &TrackDraft) -> String {
    export_track(draft)
}

pub fn export_all_tracks(drafts: &[TrackDraft]) -> String {
    drafts
        .iter()
        .map(export_track)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn export_track(draft: &TrackDraft) -> String {
    let fields = &draft.fields;
    [
        "[[tracks]]".to_string(),
        format!("track_id = {}", quote_string(&fields.track_id)),
        format!("symbol = {}", quote_string(&fields.symbol)),
        format!("lower_price = {}", format_f64(fields.lower_price)),
        format!("upper_price = {}", format_f64(fields.upper_price)),
        format!(
            "long_exposure_units = {}",
            format_f64(fields.long_exposure_units)
        ),
        format!(
            "short_exposure_units = {}",
            format_f64(fields.short_exposure_units)
        ),
        format!(
            "notional_per_unit = {}",
            format_f64(fields.notional_per_unit)
        ),
        format!("max_notional = {}", format_f64(fields.max_notional)),
        format!(
            "min_rebalance_units = {}",
            format_f64(fields.min_rebalance_units)
        ),
        format!("leverage = {}", fields.leverage),
        format!(
            "out_of_band_policy = {}",
            quote_string(fields.out_of_band_policy.as_str())
        ),
        format!("daily_loss_limit = {}", format_f64(fields.daily_loss_limit)),
        format!("total_loss_limit = {}", format_f64(fields.total_loss_limit)),
        format!(
            "shape_family = {}",
            quote_string(fields.shape_family.as_str())
        ),
    ]
    .join("\n")
}

fn quote_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn format_f64(value: f64) -> String {
    if value == 0.0 {
        return "0.0".to_string();
    }

    if value.fract().abs() < f64::EPSILON {
        return format!("{value:.1}");
    }

    let text = value.to_string();
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}
