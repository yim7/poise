use chrono::{DateTime, Local};

pub(crate) fn format_local_timestamp_for_display(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Local};

    use super::format_local_timestamp_for_display;

    #[test]
    fn formats_rfc3339_timestamp_in_local_time_without_offset() {
        let value = "2026-04-04T08:30:00Z";
        let expected = DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        assert_eq!(format_local_timestamp_for_display(value), expected);
        assert!(!format_local_timestamp_for_display(value).contains('+'));
    }

    #[test]
    fn keeps_original_value_when_timestamp_cannot_be_parsed() {
        assert_eq!(
            format_local_timestamp_for_display("not-a-timestamp"),
            "not-a-timestamp"
        );
    }
}
