use super::*;
use crate::overview::fixtures::{application, environment};
use piqueld_client::ApplicationState;
use serde_json::json;

#[test]
fn an_application_shows_its_least_healthy_environment() {
    let row = |environments| application("app-shop0001", environments, &[]).health();
    assert_eq!(row(Vec::new()), ApplicationHealth::NotDeployed);
    assert_eq!(
        row(vec![
            environment("production", false, Some((ApplicationState::Ready, None))),
            environment("staging", false, Some((ApplicationState::Failed, None))),
            environment("qa", false, Some((ApplicationState::Degraded, None))),
        ]),
        ApplicationHealth::Failed
    );
    // A missing status is pending, never healthy.
    assert_eq!(
        row(vec![
            environment("production", false, Some((ApplicationState::Ready, None))),
            environment("staging", false, None),
        ]),
        ApplicationHealth::Pending
    );
    // A stopped environment reads as stopped whatever its last status,
    // which ranks above a healthy one but below any problem.
    assert_eq!(
        row(vec![
            environment("production", false, Some((ApplicationState::Ready, None))),
            environment("staging", true, Some((ApplicationState::Failed, None))),
        ]),
        ApplicationHealth::Stopped
    );
    assert_eq!(
        row(vec![
            environment("production", false, Some((ApplicationState::Pending, None))),
            environment("staging", true, None),
        ]),
        ApplicationHealth::Pending
    );
}

#[test]
fn the_subtitle_prefers_deletion_then_the_first_status_message() {
    let mut row = application(
        "app-shop0001",
        vec![
            environment("production", false, Some((ApplicationState::Ready, None))),
            environment("staging", false, None),
            environment(
                "qa",
                false,
                Some((ApplicationState::Failed, Some("image pull failed"))),
            ),
            environment(
                "dev",
                false,
                Some((ApplicationState::Degraded, Some("1 of 2"))),
            ),
        ],
        &[],
    );
    assert_eq!(row.subtitle(), "qa: image pull failed");
    row.application.delete_intent = true;
    assert_eq!(row.subtitle(), "Deletion requested");
    let quiet = application(
        "app-shop0001",
        vec![environment("production", false, None)],
        &[],
    );
    assert_eq!(quiet.subtitle(), "Generation 4");
}

#[test]
fn recent_deployments_are_the_newest_across_applications() {
    let rows = [
        application(
            "app-shop0001",
            Vec::new(),
            &[("op-a1", 10), ("op-a2", 50), ("op-a3", 30)],
        ),
        application("app-notes001", Vec::new(), &[("op-b1", 40), ("op-b2", 50)]),
        application("app-empty001", Vec::new(), &[]),
    ];
    let recent = ApplicationRow::recent_deployments(&rows, 4)
        .into_iter()
        .map(|(application, operation)| (application.id.as_str(), operation.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        recent,
        [
            ("app-notes001", "op-b2"),
            ("app-shop0001", "op-a2"),
            ("app-notes001", "op-b1"),
            ("app-shop0001", "op-a3"),
        ]
    );
    assert_eq!(rows[0].last_deployed_ms(), Some(50));
    assert_eq!(rows[2].last_deployed_ms(), None);
}

#[test]
fn previews_report_their_most_constrained_limit() {
    let usage = |total, applications: &[u32]| {
        serde_json::from_value::<PreviewUsage>(json!({
            "limits": { "max_total": 5, "max_per_application": 2 },
            "total": total,
            "applications": applications
                .iter()
                .enumerate()
                .map(|(index, previews)| json!({
                    "id": format!("app-shop000{index}"),
                    "name": format!("shop{index}"),
                    "previews": previews,
                }))
                .collect::<Vec<_>>(),
            "cpu_millis": 0,
            "memory_bytes": 0,
        }))
        .expect("usage")
    };
    assert_eq!(PreviewLoad::of(&usage(3, &[1, 1])), PreviewLoad::Within);
    // One application at its own limit refuses its next preview while
    // the installation still has room.
    assert_eq!(PreviewLoad::of(&usage(3, &[2, 1])), PreviewLoad::AtLimit);
    assert_eq!(PreviewLoad::of(&usage(5, &[1])), PreviewLoad::AtLimit);
    // A count over a lowered limit outweighs one at its limit.
    assert_eq!(PreviewLoad::of(&usage(5, &[3, 2])), PreviewLoad::OverLimit);
}
