//! The traffic the gateway counted on an environment's routes: visitors,
//! page views and requests over a period as a column chart, then the most
//! viewed pages, the sites that link to them, and each route's totals.
use super::super::UserError;
use super::super::observability::{Period, Refresh};
use super::super::ui::{Icon, Tone, empty, icon, loaded, metric, notice};
use super::editor;
use crate::traffic::{Count, Measure, scale_top};
use leptos::prelude::*;
use piqueld_client::Client;
use piqueld_client::observability::{
    Bytes, Elapsed, RouteTraffic, TrafficRank, TrafficReport, UtcTime,
};
use std::time::Duration;

/// Points past which the chart's columns touch, without gaps.
const DENSE: usize = 100;
/// How often the tab reads the counts again; the daemon adds new ones every
/// minute.
const REFRESH: Duration = Duration::from_mins(1);

/// The Traffic tab: the selected environment's traffic, read again every
/// minute and whenever the period or service changes.
#[component]
pub(super) fn EnvironmentTraffic() -> impl IntoView {
    let context = editor();
    let id = context.environment_id();
    let refresh = Refresh::every(REFRESH);
    let period = Period::new();
    let service = RwSignal::new(String::new());
    let measure = RwSignal::new(Measure::Visitors);
    let data = LocalResource::new(move || {
        refresh.track();
        let (id, since, service) = (id.clone(), period.since(), service.get());
        async move {
            Client::browser()
                .environment_traffic(
                    &id,
                    since,
                    None,
                    (!service.is_empty()).then_some(service.as_str()),
                    None,
                )
                .await
                .user_error()
        }
    });
    view! {
        <div class="stack">
            <div class="toolbar">
                {period.select(false, || {})}
                <label class="field">
                    <span>"Service"</span>
                    <select on:change={move |e| service.set(event_target_value(&e))}>
                        <option value="">"All services"</option>
                        {move || {
                            context
                                .environment_services()
                                .into_iter()
                                .map(|name| view! { <option value={name.clone()}>{name.clone()}</option> })
                                .collect_view()
                        }}
                    </select>
                </label>
                <div class="toolbar-end">
                    <button type="button" class="btn btn-sm" on:click={move |_| refresh.request()}>
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </div>
            </div>
            {loaded(data, "Loading traffic…", move |report| traffic(report, measure))}
        </div>
    }
}

/// The report: totals, the chart, then pages, referring sites and routes.
fn traffic(report: TrafficReport, measure: RwSignal<Measure>) -> AnyView {
    let totals = &report.totals;
    let counting = (!report.counting).then(|| {
        notice(
            Tone::Info,
            "piqueld isn't counting traffic now. Set ingress.enabled and ingress.traffic in its configuration, then restart it. Counts recorded before stay here.",
        )
    });
    if totals.requests == 0 {
        return view! {
            {counting}
            {empty(
                "No requests counted in this period. Requests to routes that send traffic to a service appear here within a minute.",
            )}
        }
        .into_any();
    }
    let errors = format!("{} server errors", Count(totals.server_errors));
    let mean = totals
        .mean_duration_ms
        .map_or_else(|| "—".into(), |ms| Elapsed::from_millis_f64(ms).to_string());
    let sent = format!("{} sent", Bytes::from_i64(totals.bytes_sent));
    let ranks = (report.pages.clone(), report.referrers.clone());
    let routes = (report.routes.len() > 1).then(|| routes(report.routes.clone()));
    view! {
        {counting}
        <div class="metrics">
            {metric(
                "Visitors a day",
                Count(report.daily_visitors()).to_string(),
                Some(format!("on average; {} over the period", Count(totals.visitors))),
            )}
            {metric("Page views", Count(totals.page_views).to_string(), Some("pages loaded in a browser"))}
            {metric("Requests", Count(totals.requests).to_string(), Some(errors))}
            {metric("Mean response", mean, Some(sent))}
        </div>
        <section class="card">
            <header>
                <div>
                    <h3>
                        {move || {
                            let per = if report.bucket_ms >= 86_400_000 { "day" } else { "hour" };
                            format!("{} per {per}", measure.get().label())
                        }}
                    </h3>
                    <p>"Times are in UTC. Bots count as requests, never as visitors or page views."</p>
                </div>
                <label class="field traffic-measure">
                    <span>"Show"</span>
                    // The report, and this picker with it, is drawn again on
                    // each refresh, so each option says whether it is the
                    // measure kept outside.
                    <select on:change={move |e| {
                        let chosen = event_target_value(&e);
                        if let Some(choice) = Measure::ALL.into_iter().find(|m| m.label() == chosen) {
                            measure.set(choice);
                        }
                    }}>
                        {Measure::ALL
                            .map(|choice| {
                                view! {
                                    <option value={choice.label()} prop:selected={move || measure.get() == choice}>
                                        {choice.label()}
                                    </option>
                                }
                            })
                            .collect_view()}
                    </select>
                </label>
            </header>
            {move || chart(&report, measure.get())}
        </section>
        <div class="traffic-ranks">
            {ranked("Top pages", "Page", "No page views in this period.", ranks.0)}
            {ranked("Referring sites", "Site", "No links from other sites in this period.", ranks.1)}
        </div>
        {routes}
    }
    .into_any()
}

/// Columns of `measure` for each point of the report's series, from one
/// baseline, under a scale from [`scale_top`]. Hovering or focusing a column
/// shows its point's counts.
fn chart(report: &TrafficReport, measure: Measure) -> AnyView {
    let daily = report.bucket_ms >= 86_400_000;
    let label = |ms: i64| {
        if daily {
            format!("{:#}", UtcTime(ms))
        } else {
            UtcTime(ms).to_string()
        }
    };
    let highest = report
        .series
        .iter()
        .map(|point| measure.of(&point.counts))
        .max()
        .unwrap_or_default();
    let top = scale_top(highest);
    let points = report.series.len();
    let columns = report
        .series
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let value = measure.of(&point.counts);
            #[expect(
                clippy::cast_precision_loss,
                reason = "a column's height needs no more than f64's 52 bits"
            )]
            let height = value as f64 * 100.0 / top as f64;
            // Tooltips near an edge open inwards, so they stay on the page.
            let side = match index * 3 / points.max(1) {
                0 => "traffic-column tip-start",
                2 => "traffic-column tip-end",
                _ => "traffic-column",
            };
            let when = label(point.start_ms);
            let described = format!(
                "{when}: {} {}",
                Count(value),
                measure.label().to_lowercase()
            );
            let counts = point.counts.clone();
            view! {
                <div class={side} tabindex="0" aria-label={described}>
                    // Set through the CSSOM: the dashboard's CSP refuses style attributes.
                    <i style:height={format!("{height:.2}%")}></i>
                    <div class="traffic-tip" aria-hidden="true">
                        <strong>{when}</strong>
                        <dl>
                            {Measure::ALL
                                .map(|row| {
                                    view! {
                                        <dt>{row.label()}</dt>
                                        <dd>{Count(row.of(&counts)).to_string()}</dd>
                                    }
                                })
                                .collect_view()}
                            <dt>"Server errors"</dt>
                            <dd>{Count(counts.server_errors).to_string()}</dd>
                        </dl>
                    </div>
                </div>
            }
        })
        .collect_view();
    let first = report.series.first().map(|point| label(point.start_ms));
    let last = report.series.last().map(|point| label(point.start_ms));
    view! {
        <div class="traffic-chart" role="group" aria-label={format!("{} per point", measure.label())}>
            <div class="traffic-scale" aria-hidden="true">
                <span class="scale-top">{format!("{:#}", Count(top))}</span>
                <span class="scale-middle">{format!("{:#}", Count(top / 2))}</span>
                <span class="scale-zero">"0"</span>
            </div>
            // A year of daily points has no room for gaps between columns.
            <div class={if points > DENSE { "traffic-plot dense" } else { "traffic-plot" }}>
                {columns}
            </div>
            <div class="traffic-axis" aria-hidden="true">
                <span>{first}</span>
                <span>{last}</span>
            </div>
        </div>
    }
    .into_any()
}

/// A card listing `ranks`, the values in a `column` and their page views.
fn ranked(
    title: &'static str,
    column: &'static str,
    none: &'static str,
    ranks: Vec<TrafficRank>,
) -> AnyView {
    view! {
        <section class="card card-flush">
            <header>
                <h3>{title}</h3>
            </header>
            {if ranks.is_empty() {
                empty(none)
            } else {
                view! {
                    <table class="table">
                        <thead>
                            <tr>
                                <th>{column}</th>
                                <th class="num">"Views"</th>
                            </tr>
                        </thead>
                        <tbody>
                            {ranks
                                .into_iter()
                                .map(|rank| {
                                    view! {
                                        <tr>
                                            <td class="traffic-value">{rank.value}</td>
                                            <td class="num">{Count(rank.page_views).to_string()}</td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
        </section>
    }
    .into_any()
}

/// A card with the totals of each route, when the environment has several.
fn routes(routes: Vec<RouteTraffic>) -> AnyView {
    view! {
        <section class="card card-flush">
            <header>
                <h3>"Routes"</h3>
            </header>
            <table class="table">
                <thead>
                    <tr>
                        <th>"Hostname"</th>
                        <th>"Service"</th>
                        <th class="num">"Visitors"</th>
                        <th class="num">"Page views"</th>
                        <th class="num">"Requests"</th>
                        <th class="num">"Server errors"</th>
                    </tr>
                </thead>
                <tbody>
                    {routes
                        .into_iter()
                        .map(|route| {
                            let counts = &route.counts;
                            view! {
                                <tr>
                                    <td class="traffic-value">{route.hostname.clone()}</td>
                                    <td>{route.service.clone()}</td>
                                    <td class="num">{Count(counts.visitors).to_string()}</td>
                                    <td class="num">{Count(counts.page_views).to_string()}</td>
                                    <td class="num">{Count(counts.requests).to_string()}</td>
                                    <td class="num">{Count(counts.server_errors).to_string()}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </section>
    }
    .into_any()
}
