//! What the Traffic tab of an environment shows: which count its chart
//! measures, how counts read, and where the chart's scale ends.
use piqueld_client::observability::TrafficCounts;
use std::fmt;

/// What the chart's columns measure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Measure {
    /// Distinct browsers with a page view.
    Visitors,
    /// Pages loaded in a browser.
    PageViews,
    /// Every request.
    Requests,
}

impl Measure {
    /// The choices, in the picker's order.
    pub const ALL: [Self; 3] = [Self::Visitors, Self::PageViews, Self::Requests];

    /// Its name, as the picker and the chart's title show it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Visitors => "Visitors",
            Self::PageViews => "Page views",
            Self::Requests => "Requests",
        }
    }

    /// Its value in `counts`.
    #[must_use]
    pub const fn of(self, counts: &TrafficCounts) -> i64 {
        match self {
            Self::Visitors => counts.visitors,
            Self::PageViews => counts.page_views,
            Self::Requests => counts.requests,
        }
    }
}

/// A count as people read it: in full with thousands separators, `1,284`,
/// or with `{:#}` compact from ten thousand on, `12.9K` or `4.2M`, as chart
/// scales show it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Count(pub i64);

impl fmt::Display for Count {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        if formatter.alternate() && n.abs() >= 10_000 {
            let (unit, suffix) = if n.abs() >= 1_000_000 {
                (1_000_000, "M")
            } else {
                (1_000, "K")
            };
            // Tenths of the unit, rounded half up; whole units drop the `.0`.
            let tenths = (n * 10 + unit / 2) / unit;
            return if tenths % 10 == 0 {
                write!(formatter, "{}{suffix}", tenths / 10)
            } else {
                write!(formatter, "{}.{}{suffix}", tenths / 10, tenths % 10)
            };
        }
        let digits = n.unsigned_abs().to_string();
        let mut grouped = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        write!(formatter, "{}{grouped}", if n < 0 { "-" } else { "" })
    }
}

/// The top of a chart's scale for a `highest` value: rounded up to a
/// multiple of its leading power of ten, or of 2 under 10, so the middle
/// gridline falls on a whole number too.
///
/// ```text
/// 0 -> 2, 7 -> 8, 531 -> 600, 1000 -> 1000, 1300 -> 2000
/// ```
#[must_use]
pub fn scale_top(highest: i64) -> i64 {
    let mut step = 1_i64;
    while step * 10 <= highest {
        step *= 10;
    }
    let step = step.max(2);
    (highest.max(1) + step - 1) / step * step
}

#[cfg(test)]
mod tests {
    use super::{Count, scale_top};

    #[test]
    fn scales_round_up_to_their_leading_power_of_ten() {
        for (highest, top) in [
            (0, 2),
            (7, 8),
            (10, 10),
            (531, 600),
            (1_000, 1_000),
            (1_300, 2_000),
        ] {
            assert_eq!(scale_top(highest), top, "{highest}");
        }
    }

    #[test]
    fn counts_group_thousands_and_compact_on_scales() {
        for (n, full, compact) in [
            (0, "0", "0"),
            (1_284, "1,284", "1,284"),
            (12_940, "12,940", "12.9K"),
            (20_000, "20,000", "20K"),
            (4_210_000, "4,210,000", "4.2M"),
            (-1_000, "-1,000", "-1,000"),
        ] {
            assert_eq!(Count(n).to_string(), full);
            assert_eq!(format!("{:#}", Count(n)), compact);
        }
    }
}
