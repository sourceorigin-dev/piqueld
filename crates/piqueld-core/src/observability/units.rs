//! Sizes and counts as people read them, as `piquelctl` and the dashboard
//! show them.
use std::fmt;

/// A quantity in tenths of a unit, rounded half up from `value` of a `unit`:
/// `Tenths::of(1536, 1024)` reads `1.5`. With `{:#}`, a whole number drops
/// its `.0`. Sizes, durations and compact counts round this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub(super) struct Tenths(u128);

impl Tenths {
    /// `value` in tenths of `unit`, which is not zero.
    pub(super) const fn of(value: u128, unit: u128) -> Self {
        Self((value * 10 + unit / 2) / unit)
    }
}

impl fmt::Display for Tenths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, tenth) = (self.0 / 10, self.0 % 10);
        if formatter.alternate() && tenth == 0 {
            write!(formatter, "{whole}")
        } else {
            write!(formatter, "{whole}.{tenth}")
        }
    }
}

/// A size as people read it, in binary units with one decimal from a
/// kibibyte: `512 B`, `1.5 KiB` or `118.3 GiB`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bytes(pub u64);

impl Bytes {
    /// From a byte count the API reports signed, such as retained build
    /// output; negative counts are zero.
    #[must_use]
    pub fn from_i64(bytes: i64) -> Self {
        Self(u64::try_from(bytes).unwrap_or(0))
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
        // The largest unit it reaches once rounded, of 2^(10 × power) bytes:
        // the one whose previous unit rounds to 1024.0, so 1023.95 KiB
        // reads as 1.0 MiB.
        match (1..=UNITS.len())
            .rev()
            .find(|power| Tenths::of(u128::from(self.0), 1 << (10 * (power - 1))) >= Tenths(10_240))
        {
            None => write!(formatter, "{} B", self.0),
            Some(power) => {
                let tenths = Tenths::of(u128::from(self.0), 1 << (10 * power));
                write!(formatter, "{tenths} {}", UNITS[power - 1])
            }
        }
    }
}

/// A count as people read it: in full with thousands separators, `1,284`,
/// or with `{:#}` compact from ten thousand on, `12.9K` or `4.2M`, as the
/// dashboard's chart scales show it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Count(pub i64);

impl fmt::Display for Count {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Both forms format the magnitude and put the sign in front.
        let sign = if self.0 < 0 { "-" } else { "" };
        let n = self.0.unsigned_abs();
        if formatter.alternate() && n >= 10_000 {
            // From 999,950 on, a thousand thousands round to a million.
            let (unit, suffix) = if n >= 999_950 {
                (1_000_000, "M")
            } else {
                (1_000, "K")
            };
            return write!(formatter, "{sign}{:#}{suffix}", Tenths::of(n.into(), unit));
        }
        let digits = n.to_string();
        let mut grouped = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        write!(formatter, "{sign}{grouped}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Bytes, Count, Tenths};

    #[test]
    fn tenths_round_half_up_and_drop_a_whole_tenth_on_request() {
        for (value, unit, text, compact) in [
            (0, 10, "0.0", "0"),
            (14, 100, "0.1", "0.1"),
            (1_049, 1_000, "1.0", "1"),
            (1_050, 1_000, "1.1", "1.1"),
            (9_950, 1_000, "10.0", "10"),
        ] {
            let tenths = Tenths::of(value, unit);
            assert_eq!(tenths.to_string(), text, "{value} / {unit}");
            assert_eq!(format!("{tenths:#}"), compact, "{value} / {unit}");
        }
    }

    #[test]
    fn bytes_use_the_largest_reached_binary_unit() {
        for (bytes, text) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1_048_524, "1023.9 KiB"),
            (1_048_525, "1.0 MiB"),
            (1_073_689_395, "1023.9 MiB"),
            (1_073_689_396, "1.0 GiB"),
            (1_099_457_940_684, "1023.9 GiB"),
            (1_099_457_940_685, "1.0 TiB"),
            (52_428_800, "50.0 MiB"),
            (127_026_911_232, "118.3 GiB"),
            (u64::MAX, "16777216.0 TiB"),
        ] {
            assert_eq!(Bytes(bytes).to_string(), text);
        }
        assert_eq!(Bytes::from_i64(-1).to_string(), "0 B");
    }

    #[test]
    fn counts_group_thousands_and_compact_on_scales() {
        for (n, full, compact) in [
            (0, "0", "0"),
            (999, "999", "999"),
            (1_284, "1,284", "1,284"),
            (9_999, "9,999", "9,999"),
            (10_000, "10,000", "10K"),
            (12_940, "12,940", "12.9K"),
            (20_000, "20,000", "20K"),
            (999_949, "999,949", "999.9K"),
            (999_950, "999,950", "1M"),
            (4_210_000, "4,210,000", "4.2M"),
            (-1_000, "-1,000", "-1,000"),
            (-12_940, "-12,940", "-12.9K"),
            (-15_000, "-15,000", "-15K"),
            (-4_210_000, "-4,210,000", "-4.2M"),
            (i64::MIN, "-9,223,372,036,854,775,808", "-9223372036854.8M"),
        ] {
            assert_eq!(Count(n).to_string(), full);
            assert_eq!(format!("{:#}", Count(n)), compact);
        }
    }
}
