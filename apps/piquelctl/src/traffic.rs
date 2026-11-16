//! `env traffic`: the requests an environment's routes served, which the
//! gateway counts, as the dashboard's Traffic tab shows them.
use crate::{
    analytics::Window,
    deployments::Target,
    environments::EnvironmentArgs,
    error::Result,
    output::{Console, reports::report},
};
use clap::Args;
use piqueld_client::{
    Client,
    observability::{Bytes, Elapsed, TrafficCounts, TrafficRank, TrafficReport, UtcTime},
};

// `env traffic` arguments; `///` on fields is user-facing help.
#[derive(Debug, Args)]
pub(crate) struct TrafficArgs {
    #[command(flatten)]
    target: EnvironmentArgs,
    /// Only the routes to this service.
    #[arg(long)]
    service: Option<String>,
    /// Only the route with this hostname, such as app.example.com or
    /// *.example.com.
    #[arg(long)]
    hostname: Option<String>,
    #[command(flatten)]
    window: Window,
}

impl TrafficArgs {
    /// Prints the environment's traffic, then warns when the daemon doesn't
    /// count it now.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (since, until) = self.window.bounds()?;
        let (application, environment) = self.target.resolve(client).await?;
        let traffic = client
            .environment_traffic(
                environment.id.as_str(),
                since,
                until,
                self.service.as_deref(),
                self.hostname.as_deref(),
            )
            .await?;
        console.emit(&TrafficView {
            traffic: &traffic,
            environment: format!(
                "{} / {}",
                application.application.metadata().name,
                environment.name
            ),
        })?;
        if !traffic.counting {
            console.warning(format_args!(
                "piqueld isn't counting traffic: it needs ingress.enabled and ingress.traffic in its configuration."
            ))?;
        }
        Ok(())
    }
}

/// `env traffic` result; human output also names the environment.
struct TrafficView<'a> {
    traffic: &'a TrafficReport,
    /// `sc2dle / production`.
    environment: String,
}

report!(TrafficView<'_> as TrafficReport = traffic, self, out, {
    let t = self.traffic;
    let daily = t.bucket_ms >= 86_400_000;
    // Times to the day for daily points, to the minute otherwise.
    let time = |ms: i64| if daily { format!("{:#}", UtcTime(ms)) } else { UtcTime(ms).to_string() };
    out.label("Environment", &self.environment)?;
    out.label("Window", format_args!("{} to {} UTC", time(t.since_ms), time(t.until_ms)))?;
    let totals = &t.totals;
    out.label("Visitors", totals.visitors)?;
    out.label("Page views", totals.page_views)?;
    out.label(
        "Requests",
        format_args!(
            "{} ({} client errors, {} server errors)",
            totals.requests, totals.client_errors, totals.server_errors
        ),
    )?;
    out.label(
        "Mean response",
        totals
            .mean_duration_ms
            .map_or_else(|| "none".to_owned(), |ms| Elapsed::from_millis_f64(ms).to_string()),
    )?;
    out.label("Sent", Bytes::from_i64(totals.bytes_sent))?;
    out.blank()?;
    let counts = |counts: &TrafficCounts| {
        [counts.visitors, counts.page_views, counts.requests, counts.server_errors].map(|n| n.to_string())
    };
    out.table(
        &t.series,
        "No points in this window.",
        [if daily { "DAY" } else { "HOUR" }, "VISITORS", "PAGE VIEWS", "REQUESTS", "SERVER ERRORS"],
        |point| {
            let [visitors, views, requests, errors] = counts(&point.counts);
            [time(point.start_ms), visitors, views, requests, errors]
        },
    )?;
    out.blank()?;
    out.table(
        &t.routes,
        "No requests in this window.",
        ["SERVICE", "HOSTNAME", "VISITORS", "PAGE VIEWS", "REQUESTS", "SERVER ERRORS"],
        |route| {
            let [visitors, views, requests, errors] = counts(&route.counts);
            [route.service.clone(), route.hostname.clone(), visitors, views, requests, errors]
        },
    )?;
    let ranks = |rank: &TrafficRank| [rank.value.clone(), rank.page_views.to_string()];
    out.blank()?;
    out.table(&t.pages, "No page views in this window.", ["PAGE", "VIEWS"], ranks)?;
    out.blank()?;
    out.table(&t.referrers, "No referring sites in this window.", ["REFERRER", "VIEWS"], ranks)
});
