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
        let sign = if self.0 < 0 { "-" } else { "" };
        let n = self.0.unsigned_abs();
        if formatter.alternate() && n >= 10_000 {
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
mod tests;
