//! The import interval of a repository (KAIROS-T-0670, COLLIERY-I-0623).
//!
//! A repository can take its open issues on a cadence, with no webhook.
//! The interval is stored in seconds (`NULL`: off). A person writes it as
//! `off`, or as a number with a unit: `90s`, `15m`, `1h`, `1d`. A plain
//! number is seconds. The shortest interval is [`MIN_INTERVAL_SECS`]. The
//! longest is [`MAX_INTERVAL_SECS`]. Pure: no I/O.

/// The shortest interval, in seconds.
pub const MIN_INTERVAL_SECS: u32 = 60;

/// The longest interval, in seconds: 30 days.
pub const MAX_INTERVAL_SECS: u32 = 30 * 24 * 60 * 60;

/// Why a value is not an interval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntervalError {
    #[error(
        "The import interval {0:?} is not valid. Send off, or a number with a unit: 90s, 15m, 1h \
         or 1d."
    )]
    Form(String),
    #[error(
        "The import interval is {0} seconds. The shortest interval is {MIN_INTERVAL_SECS} seconds (1m)."
    )]
    TooShort(u64),
    #[error(
        "The import interval is {0} seconds. The longest interval is {MAX_INTERVAL_SECS} seconds (30d)."
    )]
    TooLong(u64),
}

/// Read an interval. `Ok(None)`: off. Pure.
pub fn parse_interval(text: &str) -> Result<Option<u32>, IntervalError> {
    let value = text.trim().to_ascii_lowercase();
    if value == "off" {
        return Ok(None);
    }
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, unit) = value.split_at(split);
    let count: u64 = digits
        .parse()
        .map_err(|_| IntervalError::Form(text.to_string()))?;
    let factor: u64 = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(IntervalError::Form(text.to_string())),
    };
    let secs = count.saturating_mul(factor);
    if secs < u64::from(MIN_INTERVAL_SECS) {
        return Err(IntervalError::TooShort(secs));
    }
    if secs > u64::from(MAX_INTERVAL_SECS) {
        return Err(IntervalError::TooLong(secs));
    }
    Ok(Some(secs as u32))
}

/// Write an interval in the form that people send: the largest unit that
/// divides it. `None`: `off`. Pure.
pub fn format_interval(secs: Option<u32>) -> String {
    match secs {
        None => "off".to_string(),
        Some(s) if s % 86_400 == 0 => format!("{}d", s / 86_400),
        Some(s) if s % 3_600 == 0 => format!("{}h", s / 3_600),
        Some(s) if s % 60 == 0 => format!("{}m", s / 60),
        Some(s) => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_units() {
        assert_eq!(parse_interval("off"), Ok(None));
        assert_eq!(parse_interval(" OFF "), Ok(None));
        assert_eq!(parse_interval("15m"), Ok(Some(900)));
        assert_eq!(parse_interval("1h"), Ok(Some(3600)));
        assert_eq!(parse_interval("1d"), Ok(Some(86_400)));
        assert_eq!(parse_interval("90s"), Ok(Some(90)));
        assert_eq!(parse_interval("120"), Ok(Some(120)));
    }

    #[test]
    fn the_floor_and_the_ceiling() {
        assert_eq!(parse_interval("59s"), Err(IntervalError::TooShort(59)));
        assert_eq!(parse_interval("0"), Err(IntervalError::TooShort(0)));
        assert_eq!(parse_interval("1m"), Ok(Some(60)));
        assert_eq!(parse_interval("30d"), Ok(Some(MAX_INTERVAL_SECS)));
        assert_eq!(
            parse_interval("31d"),
            Err(IntervalError::TooLong(31 * 86_400))
        );
        assert!(matches!(
            parse_interval("99999999999999999999d"),
            Err(IntervalError::Form(_))
        ));
    }

    #[test]
    fn bad_forms_are_named() {
        for bad in ["", "m", "1w", "1.5h", "-5m", "15 m", "on", "every hour"] {
            let err = parse_interval(bad).expect_err(bad);
            assert!(matches!(err, IntervalError::Form(_)), "{bad}: {err:?}");
            assert!(err.to_string().contains(&format!("{bad:?}")), "{err}");
        }
    }

    #[test]
    fn the_text_goes_back_and_forth() {
        for text in ["off", "1m", "15m", "1h", "1d", "90s", "7d"] {
            let secs = parse_interval(text).unwrap();
            assert_eq!(format_interval(secs), text);
        }
    }
}
