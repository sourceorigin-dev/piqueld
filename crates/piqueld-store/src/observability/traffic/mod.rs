//! The traffic the gateway counts for each environment's routes: hourly
//! totals, and each day's most viewed pages and referring sites.
use crate::{Store, StoreError, days_ago_ms};
use piqueld_core::EnvironmentId;
use piqueld_core::observability::{
    RouteTraffic, TrafficCounts, TrafficPoint, TrafficRank, TrafficReport,
};
use std::collections::BTreeMap;
use std::ops::AddAssign;

/// Distinct pages, and referring sites, counted per route, day and kind.
/// Later new values that day aren't counted, so a burst of distinct paths
/// can't grow the table without bound.
const TRAFFIC_RANK_LIMIT: i64 = 100;
/// Pages, and referring sites, a report lists.
const REPORT_RANKS: i64 = 20;
/// An hour, in ms.
pub const HOUR_MS: i64 = 3_600_000;
/// A day, in ms.
pub const DAY_MS: i64 = 86_400_000;
/// The longest window a report covers: hourly points up to two days, daily
/// ones beyond.
const HOURLY_UNTIL_MS: i64 = 2 * DAY_MS;

/// One route of an environment: the service and hostname the gateway tags
/// its requests with.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrafficRoute {
    /// The environment the route belongs to.
    pub environment: EnvironmentId,
    /// The service it sends requests to.
    pub service: String,
    /// Its hostname, a wildcard's included as written, such as `*.example.com`.
    pub hostname: String,
}

/// What page views are ranked by.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RankKind {
    /// The page's path, without its query.
    Page,
    /// The hostname of the site whose link led to the page.
    Referrer,
}

impl RankKind {
    /// Its `traffic_ranks.kind`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Referrer => "referrer",
        }
    }
}

/// Request totals as stored, and summed: [`TrafficCounts`] without the mean.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HourCounts {
    /// Requests served.
    pub requests: i64,
    /// Successful requests for pages from browsers.
    pub page_views: i64,
    /// Browsers first seen that day on the route.
    pub visitors: i64,
    /// Requests answered with a 4xx status.
    pub client_errors: i64,
    /// Requests answered with a 5xx status.
    pub server_errors: i64,
    /// Response body bytes.
    pub bytes_sent: i64,
    /// Summed time to answer, in microseconds.
    pub duration_us: i64,
}

impl AddAssign for HourCounts {
    fn add_assign(&mut self, other: Self) {
        self.requests += other.requests;
        self.page_views += other.page_views;
        self.visitors += other.visitors;
        self.client_errors += other.client_errors;
        self.server_errors += other.server_errors;
        self.bytes_sent += other.bytes_sent;
        self.duration_us += other.duration_us;
    }
}

impl From<HourCounts> for TrafficCounts {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a mean in ms needs no more than f64's 52 bits"
    )]
    fn from(counts: HourCounts) -> Self {
        Self {
            requests: counts.requests,
            page_views: counts.page_views,
            visitors: counts.visitors,
            client_errors: counts.client_errors,
            server_errors: counts.server_errors,
            bytes_sent: counts.bytes_sent,
            mean_duration_ms: (counts.requests > 0)
                .then(|| counts.duration_us as f64 / counts.requests as f64 / 1000.0),
        }
    }
}

/// Counts gathered since the last [`Store::record_traffic`], which adds them
/// to the stored ones.
#[derive(Debug, Default)]
pub struct TrafficTally {
    /// Each route's counts per hour, by the hour's start in Unix ms.
    pub hours: BTreeMap<(TrafficRoute, i64), HourCounts>,
    /// Page views per route, UTC day start in Unix ms, kind and value.
    pub ranks: BTreeMap<(TrafficRoute, i64, RankKind, String), i64>,
}

impl TrafficTally {
    /// Whether nothing was counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hours.is_empty() && self.ranks.is_empty()
    }
}

/// Which traffic of an environment a report covers.
#[derive(Clone, Copy, Debug)]
pub struct TrafficQuery<'a> {
    /// Start of the window, in Unix ms; rounded down to its first point.
    pub since_ms: i64,
    /// End of the window, exclusive, in Unix ms.
    pub until_ms: i64,
    /// Only the routes to this service.
    pub service: Option<&'a str>,
    /// Only the route with this hostname.
    pub hostname: Option<&'a str>,
}

impl Store {
    /// Adds `tally` to the stored counts in one transaction. Counts of
    /// environments deleted meanwhile are dropped, and so are new pages and
    /// referring sites past [`TRAFFIC_RANK_LIMIT`].
    ///
    /// # Errors
    /// Returns a storage error; nothing is added then.
    pub async fn record_traffic(&self, tally: &TrafficTally) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        for ((route, hour), counts) in &tally.hours {
            let environment = route.environment.as_str();
            sqlx::query!(
                "INSERT INTO traffic_hours(environment_id,service,hostname,hour_ms,requests,page_views,visitors,client_errors,server_errors,bytes_sent,duration_us)
                SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11 WHERE EXISTS(SELECT 1 FROM environments WHERE id=?1)
                ON CONFLICT(environment_id,service,hostname,hour_ms) DO UPDATE SET
                requests=requests+excluded.requests,page_views=page_views+excluded.page_views,
                visitors=visitors+excluded.visitors,client_errors=client_errors+excluded.client_errors,
                server_errors=server_errors+excluded.server_errors,bytes_sent=bytes_sent+excluded.bytes_sent,
                duration_us=duration_us+excluded.duration_us",
                environment,
                route.service,
                route.hostname,
                hour,
                counts.requests,
                counts.page_views,
                counts.visitors,
                counts.client_errors,
                counts.server_errors,
                counts.bytes_sent,
                counts.duration_us,
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        for ((route, day, kind, value), page_views) in &tally.ranks {
            let (environment, kind) = (route.environment.as_str(), kind.as_str());
            sqlx::query!(
                "INSERT INTO traffic_ranks(environment_id,service,hostname,day_ms,kind,value,page_views)
                SELECT ?1,?2,?3,?4,?5,?6,?7 WHERE EXISTS(SELECT 1 FROM environments WHERE id=?1)
                AND (EXISTS(SELECT 1 FROM traffic_ranks WHERE environment_id=?1 AND service=?2 AND hostname=?3 AND day_ms=?4 AND kind=?5 AND value=?6)
                OR (SELECT COUNT(*) FROM traffic_ranks WHERE environment_id=?1 AND service=?2 AND hostname=?3 AND day_ms=?4 AND kind=?5)<?8)
                ON CONFLICT(environment_id,service,hostname,day_ms,kind,value) DO UPDATE SET page_views=page_views+excluded.page_views",
                environment,
                route.service,
                route.hostname,
                day,
                kind,
                value,
                page_views,
                TRAFFIC_RANK_LIMIT,
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Environment `id`'s traffic over `query`'s window, with `counting`
    /// unset: only the daemon knows it. Pages and referring sites cover the
    /// whole UTC days the window touches, since they're counted per day.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn traffic(
        &self,
        id: &EnvironmentId,
        query: TrafficQuery<'_>,
    ) -> Result<TrafficReport, StoreError> {
        let bucket = if query.until_ms - query.since_ms <= HOURLY_UNTIL_MS {
            HOUR_MS
        } else {
            DAY_MS
        };
        let since = query.since_ms - query.since_ms.rem_euclid(bucket);
        let until = query
            .until_ms
            .saturating_add(HOUR_MS - 1)
            .div_euclid(HOUR_MS)
            .saturating_mul(HOUR_MS)
            .max(since);
        let id = id.as_str();
        let rows = sqlx::query!(
            r#"SELECT (hour_ms-?2)/?6 AS "point!: i64",service,hostname,
            SUM(requests) AS "requests!: i64",SUM(page_views) AS "page_views!: i64",SUM(visitors) AS "visitors!: i64",
            SUM(client_errors) AS "client_errors!: i64",SUM(server_errors) AS "server_errors!: i64",
            SUM(bytes_sent) AS "bytes_sent!: i64",SUM(duration_us) AS "duration_us!: i64"
            FROM traffic_hours WHERE environment_id=?1 AND hour_ms>=?2 AND hour_ms<?3
            AND (?4 IS NULL OR service=?4) AND (?5 IS NULL OR hostname=?5)
            GROUP BY 1,service,hostname"#,
            id,
            since,
            until,
            query.service,
            query.hostname,
            bucket,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let points = usize::try_from((until - since + bucket - 1) / bucket).unwrap_or_default();
        let mut series = vec![HourCounts::default(); points];
        let mut routes = BTreeMap::<(String, String), HourCounts>::new();
        let mut totals = HourCounts::default();
        for row in rows {
            let counts = HourCounts {
                requests: row.requests,
                page_views: row.page_views,
                visitors: row.visitors,
                client_errors: row.client_errors,
                server_errors: row.server_errors,
                bytes_sent: row.bytes_sent,
                duration_us: row.duration_us,
            };
            let point = usize::try_from(row.point).map_err(StoreError::corrupt)?;
            *series.get_mut(point).ok_or(StoreError::Corrupt)? += counts;
            *routes.entry((row.service, row.hostname)).or_default() += counts;
            totals += counts;
        }
        let mut routes: Vec<RouteTraffic> = routes
            .into_iter()
            .map(|((service, hostname), counts)| RouteTraffic {
                service,
                hostname,
                counts: counts.into(),
            })
            .collect();
        routes.sort_by_key(|route| std::cmp::Reverse(route.counts.requests));
        let (pages, referrers) = self.ranks(id, query, since, until).await?;
        Ok(TrafficReport {
            since_ms: since,
            until_ms: until,
            bucket_ms: bucket,
            counting: false,
            totals: totals.into(),
            series: (since..)
                .step_by(usize::try_from(bucket).map_err(StoreError::corrupt)?)
                .zip(series)
                .map(|(start_ms, counts)| TrafficPoint {
                    start_ms,
                    counts: counts.into(),
                })
                .collect(),
            routes,
            pages,
            referrers,
        })
    }

    /// The most viewed pages, and referring sites, of environment `id`'s
    /// routes `query` selects, on the UTC days from `since` until `until`.
    async fn ranks(
        &self,
        id: &str,
        query: TrafficQuery<'_>,
        since: i64,
        until: i64,
    ) -> Result<(Vec<TrafficRank>, Vec<TrafficRank>), StoreError> {
        let first_day = since - since.rem_euclid(DAY_MS);
        let rows = sqlx::query!(
            r#"SELECT kind,value,page_views AS "page_views!: i64" FROM (
                SELECT kind,value,SUM(page_views) AS page_views,
                ROW_NUMBER() OVER (PARTITION BY kind ORDER BY SUM(page_views) DESC,value) AS position
                FROM traffic_ranks WHERE environment_id=?1 AND day_ms>=?2 AND day_ms<?3
                AND (?4 IS NULL OR service=?4) AND (?5 IS NULL OR hostname=?5)
                GROUP BY kind,value
            ) WHERE position<=?6 ORDER BY kind,position"#,
            id,
            first_day,
            until,
            query.service,
            query.hostname,
            REPORT_RANKS,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let (mut pages, mut referrers) = (Vec::new(), Vec::new());
        for row in rows {
            let rank = TrafficRank {
                value: row.value,
                page_views: row.page_views,
            };
            match row.kind.as_str() {
                "page" => pages.push(rank),
                "referrer" => referrers.push(rank),
                _ => return Err(StoreError::Corrupt),
            }
        }
        Ok((pages, referrers))
    }

    /// Removes traffic counts older than `retention.traffic_days`; zero days
    /// keeps them.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_traffic(&self) -> Result<(), StoreError> {
        if self.traffic_days == 0 {
            return Ok(());
        }
        let cutoff = days_ago_ms(self.traffic_days);
        let mut tx = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM traffic_hours WHERE hour_ms<?1", cutoff)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM traffic_ranks WHERE day_ms<=?1-?2",
            cutoff,
            DAY_MS
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}

#[cfg(test)]
mod tests;
