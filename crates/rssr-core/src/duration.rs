use std::time::Duration;

use chrono::{DateTime, Utc};

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

    // A window measured in years of seconds is a typo, not a request. Wrapping
    // it into a small number would quietly delete far more than was asked for.
    amount
        .checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| Error::Usage(format!("duration out of range: {value:?}")))
}

/// That long before now, for a retention cut-off.
pub fn ago(window: Duration) -> Result<DateTime<Utc>> {
    let out_of_range = || Error::Usage(format!("duration out of range: {}", render(window)));
    let window = chrono::Duration::from_std(window).map_err(|_| out_of_range())?;
    Utc::now()
        .checked_sub_signed(window)
        .ok_or_else(out_of_range)
}

/// The largest whole unit that describes the duration exactly, so a window
/// read back from the database is shown the way it was typed in.
pub fn render(window: Duration) -> String {
    let seconds = window.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds >= size && seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_too_large_to_hold_is_rejected_rather_than_wrapped() {
        assert!(parse("999999999999999999999d").is_err());
        assert!(parse("18446744073709551d").is_err());
    }

    #[test]
    fn a_window_is_rendered_in_the_largest_unit_that_fits_it() {
        assert_eq!(render(Duration::from_secs(15 * 86_400)), "15d");
        assert_eq!(render(Duration::from_secs(6 * 3_600)), "6h");
        assert_eq!(render(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(render(Duration::from_secs(45)), "45s");
        assert_eq!(render(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn every_window_round_trips_through_its_own_rendering() {
        for secs in [45, 90 * 60, 6 * 3_600, 15 * 86_400] {
            let window = Duration::from_secs(secs);
            assert_eq!(parse(&render(window)).unwrap(), window);
        }
    }

    #[test]
    fn a_cut_off_is_the_window_back_from_now() {
        let before = ago(Duration::from_secs(86_400)).unwrap();
        let elapsed = (Utc::now() - before).num_seconds();
        assert!((86_395..=86_405).contains(&elapsed), "{elapsed}");
    }

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
