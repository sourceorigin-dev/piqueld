use super::*;
use crate::{now_ms, testing::template};

/// The `web` service's route on `hostname` of environment `id`.
fn route(id: &EnvironmentId, hostname: &str) -> TrafficRoute {
    TrafficRoute {
        environment: id.clone(),
        service: "web".into(),
        hostname: hostname.into(),
    }
}

/// A temporary store with one environment.
async fn environment() -> (tempfile::TempDir, Store, EnvironmentId) {
    let (directory, store) = Store::temporary().await;
    let saved = store.save_template(template("site", "[spec]"), false).await;
    let id = store.saved_production(&saved).await;
    (directory, store, id)
}

/// Every traffic of `id` from `since_ms` until `until_ms`.
fn window(since_ms: i64, until_ms: i64) -> TrafficQuery<'static> {
    TrafficQuery {
        since_ms,
        until_ms,
        service: None,
        hostname: None,
    }
}

#[tokio::test]
async fn tallies_add_up_into_hourly_and_daily_points() {
    let (_directory, store, id) = environment().await;
    let day = 20_000 * DAY_MS;
    let mut tally = TrafficTally::default();
    let views = HourCounts {
        requests: 3,
        page_views: 2,
        visitors: 1,
        duration_us: 3_000,
        ..HourCounts::default()
    };
    let failure = HourCounts {
        requests: 1,
        server_errors: 1,
        ..HourCounts::default()
    };
    tally
        .hours
        .insert((route(&id, "a.example.com"), day + HOUR_MS), views);
    tally
        .hours
        .insert((route(&id, "b.example.com"), day + 30 * HOUR_MS), failure);
    let page = (route(&id, "a.example.com"), day, RankKind::Page, "/".into());
    tally.ranks.insert(page, 2);
    store
        .record_traffic(&tally)
        .await
        .expect("the store can record traffic");
    store
        .record_traffic(&tally)
        .await
        .expect("the store can record traffic");

    let partial = store
        .traffic(&id, window(day, day + HOUR_MS + 1))
        .await
        .expect("the store can traffic");
    assert_eq!(
        (partial.until_ms, partial.totals.requests),
        (day + 2 * HOUR_MS, 6)
    );
    let latest = store
        .traffic(&id, window(i64::MAX - DAY_MS, i64::MAX))
        .await
        .expect("the store can traffic");
    assert_eq!(latest.until_ms % HOUR_MS, 0);
    let report = store
        .traffic(&id, window(day, day + 2 * DAY_MS))
        .await
        .expect("the store can traffic");
    assert_eq!((report.bucket_ms, report.series.len()), (HOUR_MS, 48));
    assert_eq!(report.series[1].counts.requests, 6);
    assert_eq!(report.series[30].counts.server_errors, 2);
    assert_eq!(
        (
            report.totals.requests,
            report.totals.page_views,
            report.totals.visitors
        ),
        (8, 4, 2)
    );
    assert_eq!(report.totals.mean_duration_ms, Some(0.75));
    let hostnames: Vec<_> = report.routes.iter().map(|r| r.hostname.as_str()).collect();
    assert_eq!(hostnames, ["a.example.com", "b.example.com"]);
    let viewed = TrafficRank {
        value: "/".into(),
        page_views: 4,
    };
    assert_eq!(report.pages, [viewed]);

    let only_b = TrafficQuery {
        hostname: Some("b.example.com"),
        ..window(day + 5, day + 7 * DAY_MS)
    };
    let daily = store
        .traffic(&id, only_b)
        .await
        .expect("the store can traffic");
    assert_eq!((daily.bucket_ms, daily.since_ms), (DAY_MS, day));
    assert_eq!(daily.series.len(), 7);
    assert_eq!(daily.series[1].counts.requests, 2);
    assert_eq!(daily.totals.requests, 2);
    assert_eq!(daily.pages, []);
}

#[tokio::test]
async fn ranks_stop_growing_at_their_limit_and_old_counts_are_pruned() {
    let (_directory, store, id) = environment().await;
    let now = now_ms();
    let today = now - now.rem_euclid(DAY_MS);
    let page = |value: String| (route(&id, "a.example.com"), today, RankKind::Page, value);
    let mut tally = TrafficTally::default();
    for index in 0..=TRAFFIC_RANK_LIMIT {
        tally.ranks.insert(page(format!("/{index}")), 1);
    }
    store
        .record_traffic(&tally)
        .await
        .expect("the store can record traffic");
    let mut again = TrafficTally::default();
    again.ranks.insert(page("/0".into()), 5);
    store
        .record_traffic(&again)
        .await
        .expect("the store can record traffic");
    let ranks = sqlx::query_scalar!("SELECT COUNT(*) FROM traffic_ranks")
        .fetch_one(&store.pool)
        .await
        .expect("the row is present");
    assert_eq!(ranks, TRAFFIC_RANK_LIMIT);
    let report = store
        .traffic(&id, window(today, today + DAY_MS))
        .await
        .expect("the store can traffic");
    assert_eq!(report.pages.len(), 20);
    assert_eq!(
        (report.pages[0].value.as_str(), report.pages[0].page_views),
        ("/0", 6)
    );

    let old = today - 3 * DAY_MS;
    let mut tally = TrafficTally::default();
    let counts = HourCounts {
        requests: 1,
        ..HourCounts::default()
    };
    tally
        .hours
        .insert((route(&id, "a.example.com"), old), counts);
    tally
        .hours
        .insert((route(&id, "a.example.com"), today), counts);
    let old_page = (
        route(&id, "a.example.com"),
        old,
        RankKind::Page,
        "/old".into(),
    );
    tally.ranks.insert(old_page, 50);
    store
        .record_traffic(&tally)
        .await
        .expect("the store can record traffic");
    store.pin(|pins| pins.retention.traffic_days = Some(1));
    store
        .prune_traffic()
        .await
        .expect("the store can prune traffic");
    let report = store
        .traffic(&id, window(old, today + DAY_MS))
        .await
        .expect("the store can traffic");
    assert_eq!(report.totals.requests, 1);
    assert!(
        report.pages.iter().all(|page| page.value != "/old"),
        "ranks of pruned days are removed too"
    );
    assert_eq!(report.pages[0].value, "/0");
}
