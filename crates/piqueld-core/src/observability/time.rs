//! Unix times as UTC calendar dates, without a time zone database.
use std::fmt;

/// A time, in Unix ms, in UTC. It reads to the minute, `2026-11-16 13:05`,
/// or only its day with `{:#}`, `2026-11-16`, as `piquelctl` and the
/// dashboard show traffic windows; [`Self::rfc3339`] gives it exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UtcTime(pub i64);

/// A day, in ms.
const DAY_MS: i64 = 86_400_000;

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

#[cfg(test)]
mod tests {
    use super::UtcTime;

    #[test]
    fn times_read_as_utc_calendar_dates() {
        for (ms, minute, day, exact) in [
            (
                0,
                "1970-01-01 00:00",
                "1970-01-01",
                "1970-01-01T00:00:00.000Z",
            ),
            (
                951_782_400_000,
                "2000-02-29 00:00",
                "2000-02-29",
                "2000-02-29T00:00:00.000Z",
            ),
            (
                1_792_672_425_063,
                "2026-10-22 12:33",
                "2026-10-22",
                "2026-10-22T12:33:45.063Z",
            ),
            (
                -60_000,
                "1969-12-31 23:59",
                "1969-12-31",
                "1969-12-31T23:59:00.000Z",
            ),
        ] {
            assert_eq!(UtcTime(ms).to_string(), minute);
            assert_eq!(format!("{:#}", UtcTime(ms)), day);
            assert_eq!(UtcTime(ms).rfc3339(), exact);
        }
    }
}
