//! Times and durations as people read them, as `piquelctl` and the
//! dashboard show them: Unix times as UTC calendar dates, without a time
//! zone database, durations, and how long ago a moment was.
use super::DAY_MS;
use super::units::Tenths;
use std::{fmt, time::Duration};

/// A time, in Unix ms, in UTC. It reads to the minute, `2026-11-16 13:05`,
/// or only its day with `{:#}`, `2026-11-16`, as `piquelctl` and the
/// dashboard show traffic windows; [`Self::rfc3339`] gives it exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UtcTime(pub i64);

impl UtcTime {
    /// Its proleptic Gregorian year, month and day, from Howard Hinnant's
    /// `civil_from_days`: days counted from 1970-01-01, in 400-year eras
    /// starting on March 1st.
    const fn date(self) -> (i64, i64, i64) {
        let days = self.0.div_euclid(DAY_MS) + 719_468;
        let (era, day_of_era) = (days.div_euclid(146_097), days.rem_euclid(146_097));
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        let year = era * 400 + year_of_era + if month <= 2 { 1 } else { 0 };
        (year, month, day)
    }

    /// Milliseconds since its day's midnight.
    const fn time_of_day(self) -> i64 {
        self.0.rem_euclid(DAY_MS)
    }

    /// As an RFC 3339 timestamp to the millisecond, such as
    /// `2026-10-22T12:33:42.269Z`.
    #[must_use]
    pub fn rfc3339(self) -> String {
        let ((year, month, day), time) = (self.date(), self.time_of_day());
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            time / 3_600_000,
            time / 60_000 % 60,
            time / 1000 % 60,
            time % 1000
        )
    }
}

impl fmt::Display for UtcTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.date();
        write!(formatter, "{year:04}-{month:02}-{day:02}")?;
        if !formatter.alternate() {
            let minutes = self.time_of_day() / 60_000;
            write!(formatter, " {:02}:{:02}", minutes / 60, minutes % 60)?;
        }
        Ok(())
    }
}

/// A time the API reports in Unix milliseconds, as a UTC date and time to
/// the second: `2026-01-01 00:00:00Z`, which [`UtcTime`] reads to the
/// minute. `piquelctl` shows times this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// From Unix seconds, which account and credential expiries use.
    #[must_use]
    pub const fn from_secs(seconds: i64) -> Self {
        Self(seconds.saturating_mul(1_000))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let second = self.0.div_euclid(1_000).rem_euclid(60);
        write!(formatter, "{}:{second:02}Z", UtcTime(self.0))
    }
}

/// A long duration to the minute, as uptime reads: `3 d 4 h`, `2 h 5 min`,
/// `12 min` or, under a minute, `45 s`. The dashboard and `piquelctl` show
/// the daemon's uptime this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Uptime(pub Duration);

impl fmt::Display for Uptime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        match (
            seconds / 86_400,
            seconds % 86_400 / 3_600,
            seconds % 3_600 / 60,
        ) {
            (days @ 1.., hours, _) => write!(formatter, "{days} d {hours} h"),
            (0, hours @ 1.., minutes) => write!(formatter, "{hours} h {minutes} min"),
            (0, 0, minutes @ 1..) => write!(formatter, "{minutes} min"),
            _ => write!(formatter, "{seconds} s"),
        }
    }
}

/// A duration as people read it: `850 ms`, `3.2 s`, `4 min 12 s` or
/// `2 h 5 min`. The dashboard and `piquelctl` show durations this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elapsed(pub Duration);

impl Elapsed {
    /// From whole milliseconds, such as a difference of timestamps; negative
    /// ones are zero.
    #[must_use]
    pub fn from_millis(milliseconds: i64) -> Self {
        Self(Duration::from_millis(
            u64::try_from(milliseconds).unwrap_or(0),
        ))
    }

    /// From fractional milliseconds, such as analytics means; negative,
    /// non-finite or unrepresentable ones are zero.
    #[must_use]
    pub fn from_millis_f64(milliseconds: f64) -> Self {
        Self(Duration::try_from_secs_f64(milliseconds / 1000.0).unwrap_or_default())
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let milliseconds = self.0.as_millis();
        let seconds = (milliseconds + 500) / 1_000;
        match milliseconds {
            0..=999 => write!(formatter, "{milliseconds} ms"),
            1_000..=59_949 => write!(formatter, "{} s", Tenths::of(milliseconds, 1_000)),
            _ if seconds < 3_600 => {
                write!(formatter, "{} min {} s", seconds / 60, seconds % 60)
            }
            _ => write!(
                formatter,
                "{} h {} min",
                seconds / 3_600,
                seconds % 3_600 / 60
            ),
        }
    }
}

/// How long before now a moment is, in milliseconds; negative for a moment
/// still to come.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Age(i64);

impl Age {
    /// The age of `moment` at `now`, both in Unix milliseconds.
    #[must_use]
    pub const fn at(moment: i64, now: i64) -> Self {
        Self(now.saturating_sub(moment))
    }

    /// Compact label such as "4 min ago" or "in 3 d", or `None` from a week
    /// on, where a date reads better. Minutes round to the nearest one, so
    /// the label turns to hours rather than reading "60 min".
    #[must_use]
    pub fn label(self) -> Option<String> {
        let future = self.0 < 0;
        let seconds = self.0.unsigned_abs() / 1000;
        let text = match seconds {
            0..=44 => {
                return Some(if future { "in a moment" } else { "just now" }.into());
            }
            45..=3_569 => format!("{} min", (seconds + 30) / 60),
            3_570..=86_399 => format!("{} h", (seconds / 3_600).max(1)),
            86_400..=604_799 => format!("{} d", seconds / 86_400),
            _ => return None,
        };
        Some(if future {
            format!("in {text}")
        } else {
            format!("{text} ago")
        })
    }
}

#[cfg(test)]
mod tests;
