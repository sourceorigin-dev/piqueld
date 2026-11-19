//! Resolving sources before compilation, and checks at the wire boundary.

use piqueld_core::planner::{ActionKind, PlanRequest};
use piqueld_core::resource::{
    ObservedApplication, ResolutionRequirement, ResolutionSet, ResolvedSource, TaskState,
    compile_application, preview_resolution,
};
use piqueld_core::{Plan, image_repository};

use super::{application, digest, environment, instance, resolutions};

#[test]
fn image_resolution_is_the_only_pending_compilation_input() {
    let app = application();
    let missing = ResolutionSet::default();
    assert!(
        matches!(preview_resolution(&app, &missing).as_slice(), [ResolutionRequirement::ResolveImage { service, .. }] if service.as_str() == "web")
    );
    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    assert_eq!(
        desired.services[0].image.as_str(),
        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64))
    );
    assert_eq!(
        preview_resolution(&app, &resolutions()),
        [] as [piqueld_core::ResolutionRequirement; 0]
    );
}

#[test]
fn docker_registry_aliases_are_canonicalized() {
    assert_eq!(
        image_repository("index.docker.io/library/alpine:3"),
        image_repository("docker.io/library/alpine:3")
    );
    assert_eq!(
        image_repository("LOCALHOST/notes:1"),
        Some("localhost/notes".into())
    );
    assert_eq!(
        image_repository("DoCkEr.Io/alpine:3"),
        Some("docker.io/library/alpine".into())
    );
    assert_eq!(
        image_repository("INDEX.DOCKER.IO/alpine:3"),
        Some("docker.io/library/alpine".into())
    );
}

#[test]
fn preview_plans_splice_resolution_actions_first() {
    let app = application();
    let unresolved = preview_resolution(&app, &ResolutionSet::default());
    let plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved,
            desired: None,
        },
        &ObservedApplication::default(),
    );
    assert!(matches!(
        plan.actions.first().map(|a| &a.kind),
        Some(ActionKind::ResolveImage { .. })
    ));
    assert!(!plan.is_blocked());

    let desired = compile_application(&app, &environment(), instance(), &resolutions()).unwrap();
    let with_desired = Plan::from_request(
        &PlanRequest::Preview {
            unresolved: Vec::new(),
            desired: Some(desired),
        },
        &ObservedApplication::default(),
    );
    assert!(with_desired.summary().mutation_count > 0);
}

#[test]
fn wire_decoding_enforces_domain_guards() {
    // TaskState unknown strings decode to Unknown.
    let task: TaskState = serde_json::from_str("\"some-future-state\"").unwrap();
    assert_eq!(task, TaskState::Unknown);

    // ResolvedSource digest references must be immutable repository digests.
    let good = serde_json::json!({
        "type": "image",
        "requested": "nginx:1",
        "digest_reference": format!("docker.io/library/nginx@{}", digest()),
    });
    let decoded: ResolvedSource = serde_json::from_value(good).unwrap();
    assert!(decoded.digest_reference().ends_with(&digest()));

    let bad = serde_json::json!({
        "type": "image",
        "requested": "nginx:1",
        "digest_reference": "docker.io/library/nginx:1",
    });
    assert!(serde_json::from_value::<ResolvedSource>(bad).is_err());
}
