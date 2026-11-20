//! What the Traffic tab of an environment shows: which count its chart
//! measures and where the chart's scale ends.
use piqueld_client::observability::TrafficCounts;

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
    use super::scale_top;

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
}
