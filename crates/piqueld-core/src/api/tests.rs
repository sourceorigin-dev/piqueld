use super::{
    ApplicationView, BuildLogChunk, DeploymentPage, EnvironmentAccess, EnvironmentView, LogRecord,
    MountedSecret, SecretAccess, SecretMetadata, StoredSecret,
};

use crate::{ApplicationId, EnvironmentId, EnvironmentName, EnvironmentSource};

#[test]
fn incomplete_characters_wait_for_the_next_read() {
    assert_eq!(BuildLogChunk::complete_prefix(b"ok\xe2\x82"), 2);
    assert_eq!(BuildLogChunk::complete_prefix(b"\xffok\xe2\x82"), 3);
    assert_eq!(BuildLogChunk::complete_prefix("ok€".as_bytes()), 5);
}

fn environment(id: &str, name: &str) -> EnvironmentView {
    EnvironmentView {
        id: EnvironmentId::parse(id).unwrap(),
        application_id: ApplicationId::parse("app-notes-01").unwrap(),
        name: EnvironmentName::parse(name).unwrap(),
        source: EnvironmentSource::Saved,
        kind: crate::EnvironmentKind::Environment,
        sync: true,
        synced: None,
        telemetry: false,
        run_state: crate::api::RunState::Running,
        resolved_generation: None,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn stable_ids_select_before_names_that_look_like_them() {
    let manifest = crate::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec]",
    )
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let view = ApplicationView {
        spec_hash: manifest.spec_hash(),
        application: manifest,
        generation: 1,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
        // Name order puts the impostor first.
        environments: vec![
            environment("env-impostor-01", "app-notes-01"),
            environment("app-notes-01", "production"),
        ],
        previews: Vec::new(),
        drift: Vec::new(),
        sync_check: None,
        repository: None,
    };
    assert_eq!(
        view.environment("app-notes-01").unwrap().name.as_str(),
        "production"
    );
    assert_eq!(
        view.environment("production").unwrap().id.as_str(),
        "app-notes-01"
    );
}

/// Each mounted secret reports what its next deployment would find, so
/// failures show before deploying.
#[test]
fn mounted_secrets_report_what_deploying_would_find() {
    let manifest = crate::manifest::parse_template_toml(
        r"api_version='piqueld.dev/v1alpha1'
kind='Application'
[metadata]
name='notes'
[[spec.services]]
name='web'
source={type='image',image='nginx:alpine'}
secrets=[{name='session',target='/run/secrets/1'},{name='stripe',target='/run/secrets/2'},{name='other',target='/run/secrets/3'},{name='discarded',target='/run/secrets/4'},{name='unset',target='/run/secrets/5'}]
[[spec.secrets]]
name='session'
generate={type='random',bytes=16}
",
    )
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let production = environment("app-notes-01", "production");
    let stored = |name: &str, environments, unavailable| StoredSecret {
        access: SecretAccess {
            environments,
            previews: false,
        },
        metadata: SecretMetadata {
            name: name.into(),
            generation: 3,
            updated_at_ms: 1,
            deleting: false,
            unavailable,
        },
    };
    let others = EnvironmentAccess::Only([EnvironmentId::parse("env-staging-01").unwrap()].into());
    let stored = [
        stored("stripe", EnvironmentAccess::All, false),
        stored("other", others, false),
        stored("discarded", EnvironmentAccess::All, true),
    ];
    assert_eq!(
        MountedSecret::list(&manifest, &production, &stored),
        [
            ("discarded".into(), MountedSecret::Unavailable),
            ("other".into(), MountedSecret::Denied),
            ("session".into(), MountedSecret::Generated),
            ("stripe".into(), MountedSecret::Stored { generation: 3 }),
            ("unset".into(), MountedSecret::Missing),
        ]
    );
}

#[test]
fn terminal_controls_are_removed_but_text_and_lines_survive() {
    assert_eq!(
        LogRecord::clean_message("\x1b[31mred\x1b[0m\r\0\ttext\n"),
        "red\ttext\n"
    );
    assert_eq!(
        LogRecord::clean_message(
            "\x1b]0;hidden title\x07text \x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\"
        ),
        "text link"
    );
}

/// A whole page also matches the summary schema, and an empty page matches
/// both, so a deployment page is documented as `anyOf`, never `oneOf`.
#[test]
fn deployment_pages_are_documented_as_any_of() {
    let schema = serde_json::to_value(<DeploymentPage as utoipa::PartialSchema>::schema())
        .expect("schema JSON");
    assert!(schema.get("oneOf").is_none());
    assert_eq!(schema["anyOf"].as_array().map(Vec::len), Some(2));
}
