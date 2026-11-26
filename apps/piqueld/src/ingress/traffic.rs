//! Counts the requests the gateway serves on each environment's routes.
//!
//! Caddy writes one JSON access log line per request to [`TRAFFIC_SOCKET`]
//! in its control mount (see `configuration`). Each route that proxies to a
//! service tags its requests with `piqueld_environment`, `piqueld_service`
//! and `piqueld_hostname`; untagged lines aren't counted.
//!
//! ```text
//! {"ts":1794834486832631131,"request":{"client_ip":"203.0.113.7","method":"GET",
//!  "host":"app.example.com","uri":"/play?x=1","headers":{"User-Agent":["…"]}},
//!  "duration":49122,"size":15,"status":200,"resp_headers":{"Content-Type":["text/html"]},
//!  "piqueld_environment":"env-…","piqueld_service":"web","piqueld_hostname":"app.example.com"}
//! ```
//!
//! Lines are added up in memory and flushed to the store every minute, and
//! on shutdown, as hourly [`TrafficTally`] rows. Nothing about a single
//! request is kept: visitors are told apart by a hash of their address and
//! user agent, keyed by a random key replaced every UTC day.
use piqueld_core::EnvironmentId;
use piqueld_core::wait;
use piqueld_store::Store;
use piqueld_store::traffic::{DAY_MS, HOUR_MS, HourCounts, RankKind, TrafficRoute, TrafficTally};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

/// Socket in the control mount Caddy writes access log lines to.
pub(super) const TRAFFIC_SOCKET: &str = "traffic.sock";
/// Name of Caddy's access logger for routed requests, as the servers'
/// `default_logger_name` and the `traffic` log's include filter use it.
pub(super) const ACCESS_LOGGER: &str = "http.log.access.traffic";
/// How often counts are added to the store.
const FLUSH_INTERVAL: Duration = Duration::from_mins(1);
/// Pause before accepting again after the gateway's access log socket fails.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
/// The longest access log line read; longer ones, with huge headers, are
/// skipped.
const MAX_LINE: u64 = 1024 * 1024;
/// Visitors remembered per day. Past it, every visitor not yet seen counts
/// as new, so a flood of addresses can't grow memory without bound.
const MAX_VISITORS: usize = 200_000;
/// Distinct pages and referring sites counted between two flushes, across
/// every route; later new ones aren't, until the next flush, while those
/// already counted, and every total, still are. Single-page applications
/// answer every path with HTML, so a scan of random paths would otherwise
/// add a row, and a statement under the store's writer lock, per path.
const MAX_RANKS: usize = 1_000;
/// The longest page path counted; longer ones are cut.
const MAX_PATH: usize = 256;
/// User agent fragments of bots, crawlers, scripts and monitors, lowercase.
/// Their requests count, but never as page views or visitors.
const BOTS: &[&str] = &[
    "bot",
    "crawl",
    "spider",
    "slurp",
    "curl",
    "wget",
    "python",
    "go-http-client",
    "okhttp",
    "java/",
    "headless",
    "lighthouse",
    "monitor",
    "uptime",
    "scan",
    "facebookexternalhit",
    "preview",
];

/// The daemon's end of the access log socket, and what it counted since the
/// last flush.
pub struct Traffic {
    /// Socket Caddy writes its access log to.
    listener: UnixListener,
    /// Database the counts are written to.
    store: Arc<Store>,
    /// What was counted since the last flush.
    counter: Arc<parking_lot::Mutex<Counter>>,
}

impl Traffic {
    /// Counts what Caddy writes to `listener` into `store`.
    pub(super) fn new(listener: UnixListener, store: Arc<Store>) -> Self {
        Self {
            listener,
            store,
            counter: Arc::default(),
        }
    }

    /// Reads every connection Caddy opens, flushing counts every minute,
    /// until `cancellation`, then flushes what is left.
    pub async fn run(self, cancellation: CancellationToken) {
        let accept = async {
            loop {
                match self.listener.accept().await {
                    Ok((stream, _)) => {
                        let counter = Arc::clone(&self.counter);
                        let cancellation = cancellation.clone();
                        tokio::spawn(async move {
                            cancellation
                                .run_until_cancelled(Self::read(stream, &counter))
                                .await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(?error, "could not accept the gateway's access log");
                        wait::pause(ACCEPT_RETRY).await;
                    }
                }
            }
        };
        let flush = async {
            let mut interval = tokio::time::interval(FLUSH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while cancellation
                .run_until_cancelled(interval.tick())
                .await
                .is_some()
            {
                self.flush().await;
            }
        };
        tokio::join!(cancellation.run_until_cancelled(accept), flush);
        self.flush().await;
    }

    /// Counts each line of one connection until Caddy closes it.
    async fn read(stream: UnixStream, counter: &parking_lot::Mutex<Counter>) {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_LINE)
                .read_until(b'\n', &mut line)
                .await;
            match read {
                Ok(0) => return,
                Ok(_) if line.last() == Some(&b'\n') => {}
                Ok(_) if (line.len() as u64) < MAX_LINE => return,
                Ok(_) => {
                    tracing::debug!("skipped an access log line longer than 1 MiB");
                    let mut rest = Vec::new();
                    if !matches!(reader.read_until(b'\n', &mut rest).await, Ok(1..)) {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    tracing::debug!(?error, "the gateway's access log connection failed");
                    return;
                }
            }
            match serde_json::from_slice::<AccessLine>(&line) {
                Ok(access) => counter.lock().count(&access),
                Err(error) => tracing::debug!(?error, "skipped an unreadable access log line"),
            }
        }
    }

    /// Adds what was counted since the last flush to the store. On failure
    /// the counts are dropped, with a warning: traffic counts are an
    /// estimate, never worth holding the store up for.
    async fn flush(&self) {
        let tally = std::mem::take(&mut self.counter.lock().tally);
        if tally.is_empty() {
            return;
        }
        if let Err(error) = self.store.record_traffic(&tally).await {
            tracing::warn!(?error, "could not record traffic counts; they are dropped");
        }
    }
}

/// What was counted since the last flush, and the visitors seen today.
#[derive(Default)]
struct Counter {
    /// What was counted since the last flush.
    tally: TrafficTally,
    /// The UTC day start, in Unix ms, that `keys` and `visitors` cover.
    day: i64,
    /// Keys today's visitor hashes. Replaced each day, so a visitor can't be
    /// followed from one day to the next.
    keys: RandomState,
    /// Hashes of the route, address and user agent of today's visitors.
    visitors: HashSet<u64>,
}

impl Counter {
    /// Adds one request to the tally, if a route tagged it.
    fn count(&mut self, access: &AccessLine) {
        let Some(route) = access.route() else {
            return;
        };
        let at = access.ts / 1_000_000;
        let (hour, day) = (at - at.rem_euclid(HOUR_MS), at - at.rem_euclid(DAY_MS));
        if day > self.day {
            self.day = day;
            self.keys = RandomState::new();
            self.visitors.clear();
        }
        let mut counts = HourCounts {
            requests: 1,
            client_errors: i64::from((400..500).contains(&access.status)),
            server_errors: i64::from(access.status >= 500),
            bytes_sent: access.size.max(0),
            duration_us: access.duration.max(0) / 1000,
            ..HourCounts::default()
        };
        if access.page_view() {
            counts.page_views = 1;
            let visitor = self.keys.hash_one((
                &route,
                &access.request.client_ip,
                access.request.headers.get("User-Agent"),
            ));
            let full = self.visitors.len() >= MAX_VISITORS;
            if (full && !self.visitors.contains(&visitor)) || self.visitors.insert(visitor) {
                counts.visitors = 1;
            }
            let ranks = &mut self.tally.ranks;
            let mut rank = |kind, value: String| {
                let key = (route.clone(), day, kind, value);
                if ranks.len() < MAX_RANKS || ranks.contains_key(&key) {
                    *ranks.entry(key).or_default() += 1;
                }
            };
            rank(RankKind::Page, access.path());
            if let Some(referrer) = access.referrer() {
                rank(RankKind::Referrer, referrer);
            }
        }
        *self.tally.hours.entry((route, hour)).or_default() += counts;
    }
}

/// The fields of a Caddy access log line that are counted.
#[derive(Deserialize)]
struct AccessLine {
    /// When the request finished, in Unix ns.
    ts: i64,
    /// The request that was logged.
    request: LoggedRequest,
    /// Time to answer, in ns.
    duration: i64,
    /// Response body bytes.
    size: i64,
    /// HTTP status of the response.
    status: u16,
    /// Response headers.
    #[serde(default)]
    resp_headers: Headers,
    /// Environment the gateway routed the request to, if known.
    piqueld_environment: Option<String>,
    /// Service the gateway routed the request to, if known.
    piqueld_service: Option<String>,
    /// Route hostname that matched, if any.
    piqueld_hostname: Option<String>,
}

/// The request of an [`AccessLine`].
#[derive(Deserialize)]
struct LoggedRequest {
    /// The client's address: the peer's, the tailnet client's from the apps
    /// node's PROXY header, or `Cf-Connecting-IP` through the tunnel.
    client_ip: String,
    /// HTTP method.
    method: String,
    /// Host the client requested.
    host: String,
    /// Path and query.
    uri: String,
    /// Request headers.
    #[serde(default)]
    headers: Headers,
}

/// Logged headers, by canonical name, such as `User-Agent`. Caddy redacts
/// cookies and credentials.
#[derive(Default, Deserialize)]
#[serde(transparent)]
struct Headers(HashMap<String, Vec<String>>);

impl Headers {
    /// The first value of header `name`.
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.first().map(String::as_str)
    }
}

impl AccessLine {
    /// The route that tagged this request; `None` for untagged ones, which
    /// aren't counted.
    fn route(&self) -> Option<TrafficRoute> {
        Some(TrafficRoute {
            environment: EnvironmentId::parse(self.piqueld_environment.clone()?).ok()?,
            service: self.piqueld_service.clone()?,
            hostname: self.piqueld_hostname.clone()?,
        })
    }

    /// Whether a bot, crawler, script or monitor sent it, by its user agent;
    /// one without a user agent is too.
    fn bot(&self) -> bool {
        self.request.headers.get("User-Agent").is_none_or(|agent| {
            let agent = agent.to_ascii_lowercase();
            BOTS.iter().any(|bot| agent.contains(bot))
        })
    }

    /// Whether this is a browser loading a page: a successful `GET` for a
    /// document, as `Sec-Fetch-Dest` says, or, from browsers that don't send
    /// it, answered with HTML. Prefetches and prerenders, which
    /// `Sec-Purpose` marks, may never be seen, so they aren't.
    fn page_view(&self) -> bool {
        let document = match self.request.headers.get("Sec-Fetch-Dest") {
            Some(destination) => destination == "document",
            None => self
                .resp_headers
                .get("Content-Type")
                .is_some_and(|kind| kind.starts_with("text/html")),
        };
        self.request.method == "GET"
            && (matches!(self.status, 200..300) || self.status == 304)
            && document
            && self.request.headers.get("Sec-Purpose").is_none()
            && !self.bot()
    }

    /// The path, without its query, cut to [`MAX_PATH`] bytes.
    fn path(&self) -> String {
        let path = self.request.uri.split(['?', '#']).next().unwrap_or("/");
        let mut end = path.len().min(MAX_PATH);
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        path[..end].to_owned()
    }

    /// The hostname of the site whose link led here, from `Referer`, unless
    /// it is this request's own host or an IP address. Browsers don't land on
    /// sites by bare address, so those are bots spoofing the header.
    fn referrer(&self) -> Option<String> {
        let referrer = url::Url::parse(self.request.headers.get("Referer")?).ok()?;
        let url::Host::Domain(host) = referrer.host()? else {
            return None;
        };
        let host = host.to_ascii_lowercase();
        let own = self.request.host.split(':').next().unwrap_or_default();
        (!host.eq_ignore_ascii_case(own)).then_some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An access line Caddy writes for `uri`, tagged for environment
    /// `env-sc2dle`'s `web` service, with `headers`.
    fn line(uri: &str, status: u16, headers: &str, response_type: &str) -> AccessLine {
        serde_json::from_str(&format!(
            r#"{{"level":"info","ts":1794834486832631131,"logger":"http.log.access.traffic","msg":"handled request",
            "request":{{"remote_ip":"172.17.0.1","remote_port":"45608","client_ip":"203.0.113.7","proto":"HTTP/2.0",
            "method":"GET","host":"app.example.com","uri":"{uri}","headers":{{{headers}}}}},
            "bytes_read":0,"user_id":"","duration":2000000,"size":15,"status":{status},
            "resp_headers":{{"Content-Type":["{response_type}"]}},
            "piqueld_environment":"env-sc2dle","piqueld_service":"web","piqueld_hostname":"app.example.com"}}"#
        ))
        .expect("the value deserializes")
    }

    const BROWSER: &str =
        r#""User-Agent":["Mozilla/5.0 Firefox/150.0"],"Sec-Fetch-Dest":["document"]"#;

    #[test]
    fn page_views_need_a_browser_loading_a_document_successfully() {
        assert!(line("/", 200, BROWSER, "text/html").page_view());
        assert!(line("/", 304, BROWSER, "").page_view());
        let fetch = r#""User-Agent":["Mozilla/5.0"],"Sec-Fetch-Dest":["empty"]"#;
        assert!(!line("/api/guess", 200, fetch, "text/html").page_view());
        assert!(!line("/missing", 404, BROWSER, "text/html").page_view());
        let bot = r#""User-Agent":["Googlebot/2.1"],"Sec-Fetch-Dest":["document"]"#;
        assert!(!line("/", 200, bot, "text/html").page_view());
        let prefetch = format!(r#"{BROWSER},"Sec-Purpose":["prefetch"]"#);
        assert!(!line("/", 200, &prefetch, "text/html").page_view());
        let old = r#""User-Agent":["Mozilla/5.0"]"#;
        assert!(line("/", 200, old, "text/html; charset=utf-8").page_view());
        assert!(!line("/app.js", 200, old, "text/javascript").page_view());
    }

    #[test]
    fn pages_drop_their_query_and_referrers_their_own_host() {
        let referred = |referrer: &str| {
            line(
                "/play?day=3",
                200,
                &format!(r#""Referer":["{referrer}"]"#),
                "",
            )
        };
        let access = referred("https://News.example.org/item?id=1");
        assert_eq!(access.path(), "/play");
        assert_eq!(access.referrer().as_deref(), Some("news.example.org"));
        assert_eq!(referred("https://app.example.com/other").referrer(), None);
        assert_eq!(referred("not a url").referrer(), None);
        assert_eq!(referred("https://104.21.6.75/").referrer(), None);
        assert_eq!(referred("http://[2606:4700::1]/").referrer(), None);
    }

    #[test]
    fn ranks_stop_at_their_limit_between_flushes() {
        let mut counter = Counter::default();
        for index in 0..MAX_RANKS {
            counter.count(&line(&format!("/{index}"), 200, BROWSER, "text/html"));
        }
        counter.count(&line("/new", 200, BROWSER, "text/html"));
        counter.count(&line("/0", 200, BROWSER, "text/html"));
        let ranks = &counter.tally.ranks;
        assert_eq!(ranks.len(), MAX_RANKS);
        assert!(ranks.keys().all(|key| key.3 != "/new"));
        let first = ranks
            .iter()
            .find(|(key, _)| key.3 == "/0")
            .expect("the match exists");
        assert_eq!(*first.1, 2);
        assert_eq!(
            counter
                .tally
                .hours
                .values()
                .next()
                .expect("the next item exists")
                .page_views,
            1_002
        );
    }

    #[test]
    fn counts_add_up_per_hour_and_visitors_once_a_day() {
        let mut counter = Counter::default();
        let page = line("/", 200, BROWSER, "text/html");
        counter.count(&page);
        counter.count(&page);
        counter.count(&line("/api", 500, "", "application/json"));
        let mut probe = line("/.well-known/piqueld-ingress", 200, "", "text/plain");
        probe.piqueld_environment = None;
        counter.count(&probe);
        let [(_, counts)] = counter.tally.hours.iter().collect::<Vec<_>>()[..] else {
            panic!("one hour of one route");
        };
        assert_eq!(
            *counts,
            HourCounts {
                requests: 3,
                page_views: 2,
                visitors: 1,
                client_errors: 0,
                server_errors: 1,
                bytes_sent: 45,
                duration_us: 6000,
            }
        );
        let pages: Vec<_> = counter.tally.ranks.iter().collect();
        assert_eq!(pages.len(), 1);
        assert_eq!((pages[0].0.3.as_str(), *pages[0].1), ("/", 2));
    }
}
