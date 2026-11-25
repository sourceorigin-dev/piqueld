//! The traffic of an environment's routes, which the gateway counts.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Requests an environment's routes served over a window, as hourly totals
/// the gateway counted. Requests to redirect routes, and the gateway's own
/// probes, aren't counted.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct TrafficReport {
    /// Start of the window, inclusive, in Unix ms: the start of its first
    /// point.
    pub since_ms: i64,
    /// End of the window, exclusive, in Unix ms: the end of the hour the
    /// requested end falls in, since counts are hourly.
    pub until_ms: i64,
    /// Length of each `series` point in ms: an hour for windows of up to two
    /// days, a day for longer ones.
    pub bucket_ms: i64,
    /// Whether the daemon counts traffic now, with `ingress.enabled` and
    /// `ingress.traffic` set. Counts recorded before stay readable.
    pub counting: bool,
    /// Totals over the window.
    pub totals: TrafficCounts,
    /// Totals of each `bucket_ms` of the window, oldest first, without gaps.
    pub series: Vec<TrafficPoint>,
    /// Totals of each route that served requests, most requests first.
    pub routes: Vec<RouteTraffic>,
    /// The 20 most viewed pages, by path without the query, most views
    /// first. Pages are counted per UTC day, so they cover the whole days the
    /// window touches.
    pub pages: Vec<TrafficRank>,
    /// The 20 other sites whose links led to the most page views, by
    /// hostname, most views first, over the same days as `pages`.
    pub referrers: Vec<TrafficRank>,
}

impl TrafficReport {
    /// Visitors on an average day, to the nearest one, from the first point
    /// with requests, so days before counting started or after retention
    /// pruned them don't lower it, until the end of the window, and over at
    /// least a day. A quiet start of the window counts as not started. `totals.visitors` adds up each day's, so a person coming
    /// back every day of a 30-day window counts 30 times there, once here.
    #[must_use]
    pub fn daily_visitors(&self) -> i64 {
        const DAY_MS: i64 = 86_400_000;
        let first = self
            .series
            .iter()
            .find(|point| point.counts.requests > 0)
            .map_or(self.since_ms, |point| point.start_ms);
        let length = (self.until_ms - first).max(DAY_MS);
        (self.totals.visitors * DAY_MS + length / 2) / length
    }
}

#[cfg(test)]
mod tests {
    use super::{TrafficCounts, TrafficPoint, TrafficReport};

    const HOUR_MS: i64 = 3_600_000;

    /// A report from 0 until `hours`, with `visitors` in all, whose first
    /// requests came `from` hours in.
    fn report(hours: i64, visitors: i64, from: i64) -> TrafficReport {
        let counts = |requests| TrafficCounts {
            requests,
            ..TrafficCounts::default()
        };
        TrafficReport {
            until_ms: hours * HOUR_MS,
            totals: TrafficCounts {
                visitors,
                ..TrafficCounts::default()
            },
            series: (0..hours)
                .map(|hour| TrafficPoint {
                    start_ms: hour * HOUR_MS,
                    counts: counts(i64::from(hour >= from)),
                })
                .collect(),
            ..TrafficReport::default()
        }
    }

    #[test]
    fn daily_visitors_average_over_the_counted_part_of_the_window() {
        assert_eq!(report(24, 300, 0).daily_visitors(), 300);
        assert_eq!(report(25, 300, 0).daily_visitors(), 288);
        assert_eq!(report(30 * 24, 9_000, 0).daily_visitors(), 300);
        assert_eq!(report(30 * 24, 20, 0).daily_visitors(), 1);
        assert_eq!(report(30 * 24, 1_500, 25 * 24).daily_visitors(), 300);
        assert_eq!(report(1, 40, 0).daily_visitors(), 40);
        assert_eq!(report(30 * 24, 0, 30 * 24).daily_visitors(), 0);
    }
}

/// Request totals.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct TrafficCounts {
    /// Requests served, bots' included.
    pub requests: i64,
    /// Successful requests for HTML pages from browsers, not bots.
    pub page_views: i64,
    /// Distinct browsers with a page view, each counted once a day per
    /// route, in the hour of their first page view that day, so returning
    /// ones count again each day: an estimate, from their address and user
    /// agent.
    pub visitors: i64,
    /// Requests answered with a 4xx status.
    pub client_errors: i64,
    /// Requests answered with a 5xx status.
    pub server_errors: i64,
    /// Response body bytes sent.
    pub bytes_sent: i64,
    /// Mean time to answer a request, in ms; `None` without requests.
    pub mean_duration_ms: Option<f64>,
}

/// The totals of one point of a [`TrafficReport`]'s series.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct TrafficPoint {
    /// Start of the point, in Unix ms.
    pub start_ms: i64,
    /// Totals from `start_ms` until the next point.
    pub counts: TrafficCounts,
}

/// The totals of one route of an environment.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct RouteTraffic {
    /// The service the route sends requests to.
    pub service: String,
    /// The route's hostname, such as `app.example.com` or `*.example.com`.
    pub hostname: String,
    /// The route's totals over the window.
    pub counts: TrafficCounts,
}

/// One page or referring site, and its page views.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TrafficRank {
    /// A path, such as `/play`, or a hostname, such as `news.example.com`.
    pub value: String,
    /// Page views over the window.
    pub page_views: i64,
}
