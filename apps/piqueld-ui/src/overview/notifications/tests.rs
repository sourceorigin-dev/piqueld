use super::*;
use crate::overview::fixtures::application;
use serde_json::json;

/// Destination `ops` receiving build failures with a mention, and
/// deployment failures, about `shop` and its previews.
fn destination() -> NotificationDestination {
    serde_json::from_value(json!({
        "name": "ops",
        "settings": {
            "kind": "json",
            "enabled": false,
            "categories": {
                "build_failures": { "message": "<@&42>" },
                "deployment_failures": {},
            },
            "applications": { "only": ["app-shop0001"] },
            "previews": true,
        },
        "created_at_ms": 0,
        "updated_at_ms": 0,
    }))
    .expect("destination")
}

#[test]
fn an_unedited_destination_saves_its_settings_and_keeps_its_url() {
    let destination = destination();
    let draft = DestinationDraft::new(Some(&destination));
    assert!(draft.existing);
    let (name, request) = draft.request().expect("request");
    assert_eq!(name, "ops");
    assert_eq!(request.settings, destination.settings);
    assert_eq!(request.url, None);
}

#[test]
fn only_checked_categories_send_their_trimmed_messages() {
    let mut draft = DestinationDraft::new(Some(&destination()));
    draft.name = "  ops ".into();
    draft.url = " https://hooks.example.com/a ".into();
    draft
        .categories
        .remove(&NotificationCategory::BuildFailures);
    draft
        .categories
        .insert(NotificationCategory::DaemonFailures);
    draft.messages.insert(
        NotificationCategory::DeploymentFailures,
        "  on call  ".into(),
    );
    draft
        .messages
        .insert(NotificationCategory::DaemonFailures, "   ".into());
    let (name, request) = draft.request().expect("request");
    assert_eq!(name, "ops");
    assert_eq!(request.url.as_deref(), Some("https://hooks.example.com/a"));
    let messages = request
        .settings
        .categories
        .iter()
        .map(|(category, rule)| (*category, rule.message.as_ref().map(ToString::to_string)))
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            (
                NotificationCategory::DeploymentFailures,
                Some("on call".into())
            ),
            (NotificationCategory::DaemonFailures, None),
        ]
    );
}

#[test]
fn an_invalid_message_names_its_category() {
    let mut draft = DestinationDraft::new(None);
    draft.messages.insert(
        NotificationCategory::BuildFailures,
        "x".repeat(NotificationMessage::MAX_CHARS + 1),
    );
    let error = draft.request().expect_err("too long");
    assert!(error.starts_with("Build failures: "), "{error}");
    draft
        .categories
        .remove(&NotificationCategory::BuildFailures);
    draft.request().expect("`request` should succeed");
}

#[test]
fn receives_names_listed_applications_and_falls_back_to_ids() {
    let mut shop = application("app-shop0001", Vec::new(), &[]);
    shop.application.name = "shop".into();
    let applications = [shop];
    let mut settings = destination().settings;
    let text = |settings: &DestinationSettings| {
        Receives {
            settings,
            applications: &applications,
        }
        .to_string()
    };
    settings.applications = ApplicationSelection::Only(
        ["app-shop0001", "app-gone0001"]
            .map(|id| ApplicationId::parse(id).expect("id"))
            .into(),
    );
    assert_eq!(
        text(&settings),
        "2 categories · app-gone0001, shop · previews"
    );
    settings.applications = ApplicationSelection::Only(BTreeSet::new());
    settings.previews = false;
    assert_eq!(text(&settings), "2 categories · no application");
    settings.applications = ApplicationSelection::All;
    assert_eq!(text(&settings), "2 categories · every application");
    settings.categories.pop_first();
    assert_eq!(text(&settings), "1 category · every application");
}
