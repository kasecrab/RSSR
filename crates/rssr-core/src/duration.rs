use std::time::Duration;

use crate::{Error, Result};

/// Accepts `45s`, `15m`, `6h`, `7d`, and a bare number as seconds.
pub fn parse(value: &str) -> Result<Duration> {
    let value = value.trim();
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((split, _)) => value.split_at(split),
        None => (value, "s"),
    };

    let amount: u64 = digits
        .parse()
        .map_err(|_| Error::Usage(format!("not a duration: {value:?}")))?;

    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        other => {
            return Err(Error::Usage(format!(
                "unknown duration unit {other:?}; use s, m, h or d"
            )));
        }
    };

    Ok(Duration::from_secs(amount * seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_is_understood() {
        assert_eq!(parse("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(parse("7d").unwrap(), Duration::from_secs(604_800));
    }

    #[test]
    fn a_bare_number_means_seconds() {
        assert_eq!(parse("30").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn nonsense_is_rejected() {
        assert!(parse("soon").is_err());
        assert!(parse("10w").is_err());
    }
}
