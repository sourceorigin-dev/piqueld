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
    /// End of the window, exclusive, in Unix ms.
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
    /// Visitors on an average day of the window. `totals.visitors` adds up
    /// each day's, so a person coming back every day of a 30-day window
    /// counts 30 times there, once here.
    #[must_use]
    pub fn daily_visitors(&self) -> i64 {
        const DAY_MS: i64 = 86_400_000;
        let days = (self.until_ms - self.since_ms + DAY_MS - 1) / DAY_MS;
        self.totals.visitors / days.max(1)
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
    /// route, so returning ones count again each day: an estimate, from
    /// their address and user agent.
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
