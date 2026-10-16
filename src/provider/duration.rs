//! Episode-duration parsing and formatting shared by all providers: parsing
//! turns a `H:MM:SS` (or shorter) string into a [`Duration`], formatting turns
//! a seconds count into the zero-padded `HH:MM:SS` string Apple's
//! `itunes:duration` expects.
//!
//! The parse → format round-trip normalizes: an upstream part larger than
//! its natural bound (e.g. `90:30`, meaning 90 min 30 s) folds into
//! the higher unit, so every emitted value is conformant — minutes and
//! seconds always stay below 60, hours stay unbounded.

use std::time::Duration;

/// Parses a `H:MM:SS`, `M:SS` or `S` duration string into a [`Duration`].
pub(crate) fn parse_duration(duration_str: &str) -> Result<Duration, String> {
    let duration_parts: Vec<&str> = duration_str.split(':').rev().collect();

    let seconds = match duration_parts.first() {
        Some(sec_str) => sec_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let minutes = match duration_parts.get(1) {
        Some(min_str) => min_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let hours = match duration_parts.get(2) {
        Some(hour_str) => hour_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let duration_secs = hours * 3600 + minutes * 60 + seconds;
    Ok(Duration::from_secs(duration_secs))
}

/// Formats a duration in seconds as `HH:MM:SS`: minutes and seconds
/// zero-padded to two digits, hours unbounded (a 100-hour episode renders
/// as `100:00:00`).
pub(crate) fn format_duration(duration_secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        duration_secs / 3600,
        duration_secs / 60 % 60,
        duration_secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_reads_hours_minutes_seconds() {
        assert_eq!(parse_duration("1:02:03"), Ok(Duration::from_secs(3723)));
        assert_eq!(parse_duration("2:30"), Ok(Duration::from_secs(150)));
        assert_eq!(parse_duration("45"), Ok(Duration::from_secs(45)));
        assert_eq!(parse_duration("0"), Ok(Duration::from_secs(0)));
    }

    #[test]
    fn parse_duration_rejects_non_numeric_parts() {
        assert_eq!(parse_duration(""), Err("Invalid format".to_string()));
        assert_eq!(parse_duration("a:b"), Err("Invalid format".to_string()));
    }

    #[test]
    fn format_duration_pads_to_two_digits_and_keeps_hours_unbounded() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(59), "00:00:59");
        assert_eq!(format_duration(150), "00:02:30");
        assert_eq!(format_duration(3723), "01:02:03");
        assert_eq!(format_duration(360509), "100:08:29");
    }
}
