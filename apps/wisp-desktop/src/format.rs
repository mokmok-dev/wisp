//! Human-readable formatting helpers shared by the native views.
//!
//! These mirror the strings the former web UI produced so the desktop app's
//! dates, durations, and recording clock keep the same shape.

use chrono::{DateTime, Local, Utc};

/// Format elapsed milliseconds as `mm:ss` (or `h:mm:ss` past one hour).
#[must_use]
pub fn clock(total_ms: u64) -> String {
    let total_seconds = total_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Format a persisted session duration, or an em dash when unknown.
#[must_use]
pub fn duration(seconds: Option<i64>) -> String {
    match seconds {
        Some(seconds) if seconds >= 0 => clock(u64::try_from(seconds).unwrap_or_default() * 1_000),
        _ => "—".to_string(),
    }
}

/// Format a session start timestamp in the user's local timezone.
#[must_use]
pub fn started_at(started_at: DateTime<Utc>) -> String {
    started_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn clock_handles_minutes_and_hours() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(5_000), "00:05");
        assert_eq!(clock(65_000), "01:05");
        assert_eq!(clock(3_725_000), "1:02:05");
    }

    #[test]
    fn duration_renders_em_dash_when_unknown() {
        assert_eq!(duration(None), "—");
        assert_eq!(duration(Some(-1)), "—");
        assert_eq!(duration(Some(90)), "01:30");
    }

    #[test]
    fn started_at_uses_local_time() {
        let started = Utc
            .with_ymd_and_hms(2026, 7, 15, 4, 30, 0)
            .single()
            .expect("valid timestamp");
        let expected = started
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert_eq!(started_at(started), expected);
    }
}
