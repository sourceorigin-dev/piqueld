use super::*;
use crate::{
    ApplicationId, ApplicationName, EnvironmentKind, GitBranch, Preview, PreviewSlug, parse_toml,
};

/// `web` asks for 3 replicas and sets no limits; `db` sets only its CPU.
fn application() -> NormalizedApplication {
    parse_toml(
        r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
replicas = 3
[spec.services.source]
type = "image"
image = "nginx:alpine"
[[spec.services]]
name = "db"
[spec.services.source]
type = "image"
image = "postgres:17"
[spec.services.resources]
cpu_millis = 2000
"#,
    )
    .expect("the toml parses")
    .normalize(ApplicationId::parse("app-notes-01").expect("the application id is valid"))
}

fn preview() -> EnvironmentKind {
    let (name, branch) = (
        ApplicationName::parse("notes").expect("the application name is valid"),
        GitBranch::parse("feat/login").expect("the git branch is valid"),
    );
    EnvironmentKind::Preview(Box::new(Preview {
        slug: PreviewSlug::derive(&name, &branch, None),
        branch,
        slot: None,
    }))
}

#[test]
fn previews_get_default_limits_and_capped_replicas_with_warnings_and_environments_never() {
    let limits = PreviewLimits::default();
    let mut environment = application();
    assert_eq!(
        EnvironmentKind::Environment.bound(&mut environment, &limits),
        []
    );
    assert_eq!(environment, application());

    let mut bounded = application();
    let warnings = preview().bound(&mut bounded, &limits);
    let services = bounded
        .spec()
        .services
        .iter()
        .map(|service| (service.name.as_str(), service.replicas, &service.resources))
        .collect::<Vec<_>>();
    let limited = |cpu_millis| {
        Some(ValidatedResourceLimits {
            cpu_millis: Some(cpu_millis),
            memory_bytes: Some(limits.default_memory_bytes),
        })
    };
    assert_eq!(
        services,
        [("db", 1, &limited(2000)), ("web", 1, &limited(500))]
    );
    assert_eq!(
        warnings
            .iter()
            .map(|warning| (warning.resource.as_str(), warning.code.as_str()))
            .collect::<Vec<_>>(),
        [
            ("db", ApiErrorCode::PreviewLimitsDefaulted.as_str()),
            ("web", ApiErrorCode::PreviewLimitsDefaulted.as_str()),
            ("web", ApiErrorCode::PreviewReplicasCapped.as_str()),
        ]
    );
    assert_eq!(
        warnings[2].message,
        "web asks for 3 replicas; previews run at most 1 ([previews] max_replicas)"
    );
}
