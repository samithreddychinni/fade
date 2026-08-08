use std::time::Duration;

use serde::Serialize;

use crate::{FadeError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ttl {
    Duration(Duration),
    Forever,
}

impl Ttl {
    pub fn parse(input: &str) -> Result<Self> {
        let normalized = input.trim().to_ascii_lowercase();
        if normalized == "forever" {
            return Ok(Self::Forever);
        }

        Ok(Self::Duration(parse_duration(input, false)?))
    }

    pub fn ttl_seconds(self) -> Option<i64> {
        match self {
            Self::Duration(duration) => Some(duration.as_secs().min(i64::MAX as u64) as i64),
            Self::Forever => None,
        }
    }

    pub fn expires_at(self, created_at: i64) -> Result<Option<i64>> {
        let Some(ttl_seconds) = self.ttl_seconds() else {
            return Ok(None);
        };

        created_at
            .checked_add(ttl_seconds)
            .map(Some)
            .ok_or(FadeError::TimeOverflow)
    }

    pub fn label(self) -> String {
        match self {
            Self::Duration(duration) => format_duration(duration),
            Self::Forever => "forever".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DurationDisplay {
    pub seconds: u64,
    pub label: String,
}

impl DurationDisplay {
    pub fn new(duration: Duration) -> Self {
        Self {
            seconds: duration.as_secs(),
            label: format_duration(duration),
        }
    }
}

pub fn parse_duration(input: &str, allow_zero: bool) -> Result<Duration> {
    let normalized = input.trim().to_ascii_lowercase();
    if allow_zero && normalized == "0" {
        return Ok(Duration::ZERO);
    }
    let Some(unit) = normalized.chars().last() else {
        return invalid(input, "duration cannot be empty");
    };

    let multiplier = match unit {
        's' => 1_u64,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return invalid(input, "expected a duration ending in s, m, h, or d"),
    };

    let number = &normalized[..normalized.len() - unit.len_utf8()];
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return invalid(input, "expected a positive integer followed by a unit");
    }

    let value = number.parse::<u64>().map_err(|_| FadeError::InvalidDuration {
        input: input.to_string(),
        reason: "duration value is too large".to_string(),
    })?;

    if value == 0 && !allow_zero {
        return invalid(input, "duration must be greater than zero");
    }

    let seconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| FadeError::InvalidDuration {
            input: input.to_string(),
            reason: "duration value is too large".to_string(),
        })?;

    Ok(Duration::from_secs(seconds))
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        return "0s".to_string();
    }

    let days = seconds / 86_400;
    if days > 0 && seconds % 86_400 == 0 {
        return format!("{days}d");
    }

    let hours = seconds / 3_600;
    if hours > 0 && seconds % 3_600 == 0 {
        return format!("{hours}h");
    }

    let minutes = seconds / 60;
    if minutes > 0 && seconds % 60 == 0 {
        return format!("{minutes}m");
    }

    format!("{seconds}s")
}

pub fn format_ttl_seconds(ttl_seconds: Option<i64>) -> String {
    match ttl_seconds {
        Some(seconds) if seconds >= 0 => format_duration(Duration::from_secs(seconds as u64)),
        Some(seconds) => format!("{seconds}s"),
        None => "forever".to_string(),
    }
}

fn invalid<T>(input: &str, reason: &str) -> Result<T> {
    Err(FadeError::InvalidDuration {
        input: input.to_string(),
        reason: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_duration_units() {
        assert_eq!(parse_duration("1s", false).unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("15m", false).unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h", false).unwrap(), Duration::from_secs(7_200));
        assert_eq!(parse_duration("7d", false).unwrap(), Duration::from_secs(604_800));
    }

    #[test]
    fn parses_forever_ttl() {
        assert_eq!(Ttl::parse("forever").unwrap(), Ttl::Forever);
    }

    #[test]
    fn rejects_zero_ttl_but_allows_zero_recovery_duration() {
        assert!(Ttl::parse("0s").is_err());
        assert_eq!(parse_duration("0", true).unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("0s", true).unwrap(), Duration::from_secs(0));
    }

    #[test]
    fn formats_exact_duration_units() {
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_duration(Duration::from_secs(3_600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }
}
